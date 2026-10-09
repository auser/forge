//! Windows 10 1709+ / local NTFS is the supported baseline (the extended POSIX
//! rename operation is required). No pathname-based fallback: unsupported file
//! systems or security/rename operations fail closed at this boundary.
use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{ContextPlan, ContextPlanDraft, PathBuf, plan_from_draft};

// Preserve the original error and never print paths, identifiers, or contents.
// Unit-test subprocesses otherwise lose the failing OS stage at the public API.
fn stage<T>(_name: &'static str, result: io::Result<T>) -> io::Result<T> {
    #[cfg(test)]
    if let Err(error) = &result {
        eprintln!(
            "Windows context stage={_name} kind={:?} os_code={:?}",
            error.kind(),
            error.raw_os_error()
        );
    }
    result
}

fn bool_result(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn status_result(status: NTSTATUS) -> io::Result<()> {
    if status < 0 {
        // SAFETY: pure conversion of an NT status code.
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosErrorNoTeb(status) } as i32,
        ))
    } else {
        Ok(())
    }
}

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: these allocations are returned by the security APIs and
        // documented to be released using LocalFree.
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct PrivateSecurity {
    descriptor: Local,
    owner: PSID,
    default_owner: Vec<usize>,
}

impl PrivateSecurity {
    fn new() -> io::Result<Self> {
        // SAFETY: output buffers stay alive for each call. Token and allocated
        // SID strings are owned immediately and released on every exit path.
        unsafe {
            let mut token = null_mut();
            bool_result(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY,
                &mut token,
            ))?;
            let token = OwnedHandle::from_raw_handle(token);
            let mut needed = 0;
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
            bool_result(GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ))?;
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut owner_bytes = 0;
            GetTokenInformation(
                token.as_raw_handle(),
                TokenOwner,
                null_mut(),
                0,
                &mut owner_bytes,
            );
            if owner_bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut default_owner =
                vec![0usize; (owner_bytes as usize).div_ceil(size_of::<usize>())];
            bool_result(GetTokenInformation(
                token.as_raw_handle(),
                TokenOwner,
                default_owner.as_mut_ptr().cast(),
                owner_bytes,
                &mut owner_bytes,
            ))?;
            let mut sid_string = null_mut();
            bool_result(ConvertSidToStringSidW(user.User.Sid, &mut sid_string))?;
            let _sid_string = Local(sid_string.cast());
            let mut length = 0;
            while *sid_string.add(length) != 0 {
                length += 1;
            }
            let sid = String::from_utf16(std::slice::from_raw_parts(sid_string, length))
                .map_err(|_| io::ErrorKind::InvalidData)?;
            // Explicit owner and protected, non-inherited, single-user DACL.
            let sddl: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;{sid})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = null_mut();
            bool_result(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            ))?;
            let descriptor = Local(descriptor);
            let mut owner = null_mut();
            let mut defaulted = 0;
            bool_result(GetSecurityDescriptorOwner(
                descriptor.0,
                &mut owner,
                &mut defaulted,
            ))?;
            let mut dacl = null_mut();
            let mut present = 0;
            bool_result(GetSecurityDescriptorDacl(
                descriptor.0,
                &mut present,
                &mut dacl,
                &mut defaulted,
            ))?;
            if present == 0 || dacl.is_null() {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            Ok(Self {
                descriptor,
                owner,
                default_owner,
            })
        }
    }

    fn protect(&self, file: &File) -> io::Result<()> {
        // Refuse foreign-owned objects. Elevated Windows tokens can create
        // objects owned by their default owner group rather than TokenUser;
        // accept that owner too, then seal ownership to the individual user.
        // All queries/updates address the already-open handle.
        unsafe {
            let mut owner = null_mut();
            let mut descriptor = null_mut();
            let error = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            );
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            let _descriptor = Local(descriptor);
            let default_owner = &*self.default_owner.as_ptr().cast::<TOKEN_OWNER>();
            if EqualSid(owner, self.owner) == 0 && EqualSid(owner, default_owner.Owner) == 0 {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            // SetSecurityInfo propagates ACL changes to children, including
            // hardlinks we have not validated. The native operation changes
            // only this handle's object; each child is checked before sealing.
            status_result(NtSetSecurityObject(
                file.as_raw_handle(),
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                self.descriptor.0,
            ))?;
        }
        Ok(())
    }
}

fn open(
    parent: &File,
    name: &OsStr,
    directory: bool,
    disposition: u32,
    security: &PrivateSecurity,
) -> io::Result<File> {
    let mut name: Vec<u16> = name.encode_wide().collect();
    // Single components only, including exclusion of NT alternate data streams.
    if name.is_empty()
        || name.len() > 32767
        || name.iter().any(|c| matches!(*c, 0 | 47 | 58 | 92))
        || name == [46]
        || name == [46, 46]
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut unicode = UNICODE_STRING {
        Length: (name.len() * 2) as u16,
        MaximumLength: (name.len() * 2) as u16,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &mut unicode,
        Attributes: 0, // case sensitive: distinct validated IDs stay distinct
        SecurityDescriptor: security.descriptor.0.cast(),
        SecurityQualityOfService: null_mut(),
    };
    let mut handle = null_mut();
    let mut iosb = IO_STATUS_BLOCK::default();
    let access = FILE_GENERIC_READ
        | FILE_GENERIC_WRITE
        | READ_CONTROL
        | WRITE_DAC
        | WRITE_OWNER
        | if directory { 0 } else { DELETE };
    // Directory handles deny delete sharing, pinning every traversed component.
    // File readers share delete so replacement never waits for them to close.
    let sharing =
        FILE_SHARE_READ | FILE_SHARE_WRITE | if directory { 0 } else { FILE_SHARE_DELETE };
    // SAFETY: counted UTF-16 name and all structures remain alive through this
    // synchronous call. RootDirectory is a live owned handle. The final component
    // is opened as a reparse object, never followed, then checked by handle.
    unsafe {
        stage(
            "open",
            status_result(NtCreateFile(
                &mut handle,
                access,
                &attributes,
                &mut iosb,
                null(),
                FILE_ATTRIBUTE_NORMAL,
                sharing,
                disposition,
                FILE_SYNCHRONOUS_IO_NONALERT
                    | FILE_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_DIRECTORY_FILE
                    } else {
                        FILE_NON_DIRECTORY_FILE
                    },
                null(),
                0,
            )),
        )?;
        let file = File::from_raw_handle(handle);
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        bool_result(GetFileInformationByHandle(file.as_raw_handle(), &mut info))?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (!directory && info.nNumberOfLinks > 1)
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        stage("protect", security.protect(&file))?;
        Ok(file)
    }
}

// Keep all directories open for the whole operation, not just the final one.
struct Tree {
    handles: Vec<File>,
    security: PrivateSecurity,
}
impl Tree {
    fn root(path: &Path, create: bool) -> io::Result<Self> {
        let security = PrivateSecurity::new()?;
        let parts: Vec<_> = path.components().collect();
        let boundary = parts
            .iter()
            .position(|part| part.as_os_str() == ".forge")
            .unwrap_or(parts.len().saturating_sub(1));
        let anchor: PathBuf = parts[..boundary].iter().collect();
        let anchor = if anchor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &anchor
        };
        // Only the trusted project ancestry may resolve symlinks.
        let anchor = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(anchor)?;
        let mut tree = Self {
            handles: vec![anchor],
            security,
        };
        for part in &parts[boundary..] {
            match part {
                Component::Normal(name) => {
                    tree.descend(name, create)?;
                }
                Component::CurDir => {}
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            }
        }
        Ok(tree)
    }
    fn current(&self) -> &File {
        self.handles.last().unwrap()
    }
    fn descend(&mut self, name: &OsStr, create: bool) -> io::Result<()> {
        let file = open(
            self.current(),
            name,
            true,
            if create { FILE_OPEN_IF } else { FILE_OPEN },
            &self.security,
        )?;
        self.handles.push(file);
        Ok(())
    }
    fn directory(&self, name: &str, create: bool) -> io::Result<File> {
        open(
            self.current(),
            name.as_ref(),
            true,
            if create { FILE_OPEN_IF } else { FILE_OPEN },
            &self.security,
        )
    }
}

fn read<T: serde::de::DeserializeOwned>(
    parent: &File,
    name: &str,
    security: &PrivateSecurity,
) -> io::Result<Option<T>> {
    let mut file = match open(parent, name.as_ref(), false, FILE_OPEN, security) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::ErrorKind::InvalidData.into())
}

#[repr(C)]
struct RenameInformation {
    flags: u32,
    root: HANDLE,
    length: u32,
    name: [u16; 1],
}

fn replace(file: &File, parent: &File, name: &str) -> io::Result<()> {
    let name: Vec<u16> = name.encode_utf16().collect();
    let bytes = offset_of!(RenameInformation, name) + name.len() * 2;
    let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    // SAFETY: usize allocation provides native structure alignment; the trailing
    // counted UTF-16 array fits the allocation. Kernel consumes it synchronously.
    unsafe {
        let info = buffer.as_mut_ptr().cast::<RenameInformation>();
        (*info).flags = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
        (*info).root = parent.as_raw_handle();
        (*info).length = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).name.as_mut_ptr(), name.len());
        status_result(NtSetInformationFile(
            file.as_raw_handle(),
            &mut IO_STATUS_BLOCK::default(),
            info.cast(),
            bytes as u32,
            FileRenameInformationEx,
        ))
    }
}

fn write<T: serde::Serialize>(
    parent: &File,
    name: &str,
    value: &T,
    security: &PrivateSecurity,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| io::ErrorKind::InvalidData)?;
    let temporary = format!(".{}.tmp", ulid::Ulid::new());
    let mut output = open(parent, temporary.as_ref(), false, FILE_CREATE, security)?;
    let result = (|| {
        output.write_all(&bytes)?;
        output.sync_all()?;
        replace(&output, parent, name)
    })();
    if result.is_err() {
        // Delete only the temp inode we own, never re-resolve its pathname.
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                output.as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            );
        }
    }
    result
}

pub(super) fn record(path: &Path, draft: ContextPlanDraft) -> io::Result<ContextPlan> {
    let root = stage("root", Tree::root(path, true))?;
    let locks = root.directory("locks", true)?;
    let lock = open(
        &locks,
        draft.session_id.as_ref(),
        false,
        FILE_OPEN_IF,
        &root.security,
    )?;
    // Persistent lock inode: deletion would split the transaction lock domain.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return stage("lock-timeout", Err(io::ErrorKind::WouldBlock.into()));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return stage("lock", Err(error));
            }
        }
    }
    let latest = root.directory("latest", true)?;
    let latest_name = format!("{}.json", draft.session_id);
    let previous: Option<String> =
        stage("read-latest", read(&latest, &latest_name, &root.security))?;
    let changed = previous.is_some_and(|hash| hash != draft.stable_prefix.combined_hash);
    let plan = plan_from_draft(draft, changed);
    let plans = root.directory("plans", true)?;
    let run = open(
        &plans,
        plan.run_id.as_ref(),
        true,
        FILE_OPEN_IF,
        &root.security,
    )?;
    stage(
        "write-plan",
        write(
            &run,
            &format!("{}.json", plan.request_ordinal),
            &plan,
            &root.security,
        ),
    )?;
    stage(
        "write-latest",
        write(
            &latest,
            &latest_name,
            &plan.stable_prefix.combined_hash,
            &root.security,
        ),
    )?;
    Ok(plan)
}

pub(super) fn plan(path: &Path, run_id: &str, ordinal: u32) -> io::Result<Option<ContextPlan>> {
    let result = (|| {
        let mut tree = Tree::root(path, false)?;
        tree.descend("plans".as_ref(), false)?;
        tree.descend(run_id.as_ref(), false)?;
        read(tree.current(), &format!("{ordinal}.json"), &tree.security)
    })();
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ContextStore, FsContextStore, tests::draft};
    use std::fs;

    fn security_snapshot(path: &Path) -> Vec<u16> {
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        // Compare copies of the object's owner, group and DACL using the same
        // kernel-object query before and after sealing. Preserve all returned
        // ACE flags; the root positive control verifies detection of changes.
        // GetSecurityInfo produced different descendant snapshots in Windows
        // CI; the reason for that API difference has not been established.
        // SAFETY: the aligned descriptor buffer stays alive while SDDL is copied.
        unsafe {
            let information =
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
            let mut needed = 0;
            assert_eq!(
                GetKernelObjectSecurity(
                    file.as_raw_handle(),
                    information,
                    null_mut(),
                    0,
                    &mut needed,
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            let mut descriptor = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
            bool_result(GetKernelObjectSecurity(
                file.as_raw_handle(),
                information,
                descriptor.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ))
            .unwrap();
            let mut text = null_mut();
            let mut length = 0;
            bool_result(ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_mut_ptr().cast(),
                SDDL_REVISION_1,
                information,
                &mut text,
                &mut length,
            ))
            .unwrap();
            let _text = Local(text.cast());
            std::slice::from_raw_parts(text, length as usize).to_vec()
        }
    }

    fn symlink_available(result: io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32) => {
                assert!(
                    std::env::var_os("FORGE_CONTEXT_REQUIRE_SYMLINK_TESTS").is_none()
                        && std::env::var("FORGE_CONTEXT_ALLOW_SYMLINK_SKIP").as_deref() == Ok("1"),
                    "Symlink security test requires Developer Mode or SeCreateSymbolicLinkPrivilege: {error}. For an explicit local-only skip, set FORGE_CONTEXT_ALLOW_SYMLINK_SKIP=1 and run with --nocapture."
                );
                eprintln!(
                    "SKIP Windows symlink security test: ERROR_PRIVILEGE_NOT_HELD; enable Developer Mode or SeCreateSymbolicLinkPrivilege. Junction tests still run."
                );
                false
            }
            Err(error) => panic!("symlink setup failed: {error}"),
        }
    }

    #[test]
    fn directory_reparse_points_never_modify_victims() {
        for junction in [true, false] {
            for component in [
                ".forge",
                ".forge/context",
                ".forge/context/plans",
                ".forge/context/plans/run",
                ".forge/context/latest",
                ".forge/context/locks",
            ] {
                let project = tempfile::tempdir().unwrap();
                let victim = tempfile::tempdir().unwrap();
                let victim_file = victim.path().join("sentinel");
                fs::write(&victim_file, "secret victim").unwrap();
                // cmd's mklink treats forward slashes as switch delimiters.
                let link = project.path().join(component.replace('/', "\\"));
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                if junction {
                    // Junction creation does not require symlink privilege. It
                    // is deliberately not skipped on machines without that privilege.
                    let output = std::process::Command::new("cmd")
                        .args(["/D", "/C", "mklink", "/J"])
                        .arg(&link)
                        .arg(victim.path())
                        .output()
                        .unwrap();
                    assert!(output.status.success(), "junction setup: {output:?}");
                } else if !symlink_available(std::os::windows::fs::symlink_dir(
                    victim.path(),
                    &link,
                )) {
                    return;
                }
                let original_security = security_snapshot(victim.path());
                let original_file_security = security_snapshot(&victim_file);
                let store = FsContextStore::new(project.path().join(".forge/context"));
                assert!(
                    store.record(draft("run", "s", 1, "private")).is_err(),
                    "{component}"
                );
                assert_eq!(fs::read_dir(victim.path()).unwrap().count(), 1);
                assert_eq!(fs::read_to_string(&victim_file).unwrap(), "secret victim");
                assert!(
                    security_snapshot(victim.path()) == original_security,
                    "victim directory security changed: {component}"
                );
                assert!(
                    security_snapshot(&victim_file) == original_file_security,
                    "victim descendant security changed: {component}"
                );
                // Remove only the reparse point, never recurse through the target.
                fs::remove_dir(&link).unwrap();
            }
        }
    }

    #[test]
    fn file_symlinks_and_hardlinks_never_modify_victims() {
        for hardlink in [true, false] {
            for component in ["latest/s.json", "locks/s", "plans/run/1.json"] {
                let project = tempfile::tempdir().unwrap();
                let root = project.path().join("context");
                let victim = project.path().join("victim");
                fs::write(&victim, "secret victim").unwrap();
                let link = root.join(component);
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                if hardlink {
                    fs::hard_link(&victim, &link).unwrap();
                } else if !symlink_available(std::os::windows::fs::symlink_file(&victim, &link)) {
                    return;
                }
                let original_security = security_snapshot(&victim);
                let store = FsContextStore::new(&root);
                if component.starts_with("plans") {
                    assert!(store.plan("run", 1).is_err());
                    // Atomic replacement may safely replace the entry itself.
                    store.record(draft("run", "s", 1, "a")).unwrap();
                    assert!(store.plan("run", 1).unwrap().is_some());
                } else {
                    assert!(store.record(draft("run", "s", 1, "a")).is_err());
                }
                assert_eq!(fs::read_to_string(&victim).unwrap(), "secret victim");
                assert!(
                    security_snapshot(&victim) == original_security,
                    "victim file security changed: hardlink={hardlink} component={component}"
                );
            }
        }
    }

    #[test]
    fn sealing_directory_does_not_touch_unopened_descendants() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join("context");
        let nested = root.join("unopened");
        fs::create_dir_all(&nested).unwrap();
        let victim = project.path().join("victim");
        fs::write(&victim, "secret victim").unwrap();
        fs::hard_link(&victim, nested.join("link")).unwrap();
        let original_security = security_snapshot(&victim);
        let nested_security = security_snapshot(&nested);
        let root_security = security_snapshot(&root);
        let tree = Tree::root(&root, false).unwrap();
        assert_ne!(
            security_snapshot(&root),
            root_security,
            "snapshot must detect the root's deliberate security change"
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "secret victim");
        assert!(security_snapshot(&victim) == original_security);
        assert_eq!(
            String::from_utf16_lossy(&security_snapshot(&nested)),
            String::from_utf16_lossy(&nested_security),
            "sealing the root changed an unopened descendant's security"
        );
        drop(tree);
    }

    #[test]
    fn handles_pin_directories_against_substitution() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(".forge/context");
        let tree = Tree::root(&path, true).unwrap();
        assert!(fs::rename(&path, project.path().join("moved")).is_err());
        assert!(fs::rename(project.path().join(".forge"), project.path().join("moved")).is_err());
        drop(tree);
        fs::rename(&path, project.path().join("moved")).unwrap();
    }

    #[test]
    fn private_acls_and_exclusive_temporaries() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join(".forge/context");
        FsContextStore::new(&root)
            .record(draft("run", "s", 1, "private"))
            .unwrap();
        let security = PrivateSecurity::new().unwrap();
        for relative in [
            "",
            "plans",
            "plans/run",
            "latest",
            "locks",
            "plans/run/1.json",
            "latest/s.json",
            "locks/s",
        ] {
            let path = root.join(relative);
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .unwrap();
            // Inspect rather than calling the backend opener, which could repair
            // permissions and accidentally hide a creation-time ACL defect.
            unsafe {
                let mut descriptor = null_mut();
                let mut owner = null_mut();
                let mut dacl = null_mut();
                assert_eq!(
                    GetSecurityInfo(
                        file.as_raw_handle(),
                        SE_FILE_OBJECT,
                        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                        &mut owner,
                        null_mut(),
                        &mut dacl,
                        null_mut(),
                        &mut descriptor
                    ),
                    0
                );
                let _descriptor = Local(descriptor);
                assert_ne!(EqualSid(owner, security.owner), 0);
                let mut control = 0;
                let mut revision = 0;
                assert_ne!(
                    GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                    0
                );
                assert_ne!(control & SE_DACL_PROTECTED, 0);
                assert!(!dacl.is_null());
                assert_eq!((*dacl).AceCount, 1);
                let mut ace = null_mut();
                assert_ne!(GetAce(dacl, 0, &mut ace), 0);
                let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
                assert_eq!(ace.Header.AceFlags, 0);
                assert_eq!(ace.Mask, FILE_ALL_ACCESS);
                assert_ne!(
                    EqualSid(
                        (&ace.SidStart as *const u32).cast_mut().cast(),
                        security.owner
                    ),
                    0
                );
            }
        }
        let tree = Tree::root(&root, true).unwrap();
        let first = open(
            tree.current(),
            ".fixed.tmp".as_ref(),
            false,
            FILE_CREATE,
            &tree.security,
        )
        .unwrap();
        assert!(
            open(
                tree.current(),
                ".fixed.tmp".as_ref(),
                false,
                FILE_CREATE,
                &tree.security
            )
            .is_err()
        );
        drop(first);
    }
}
