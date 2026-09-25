//! Unit tests for the build script's engine resolution — the download,
//! checksum verification and caching that `build.rs` performs as resolution
//! step 3.
//!
//! `cargo test` never compiles a build script, so this file `include!`s the
//! exact source `build.rs` does (`../build_support.rs`) and drives it
//! directly. Every test is hermetic: the "mirror" is a local directory served
//! over `file://`, which `curl` speaks natively, so nothing here touches the
//! network or the real Hugging Face repo.
//!
//! What is deliberately *not* faked: the checksum. There is no env override
//! for it — a mirror has to serve the bytes forge pinned — so these tests
//! construct their own `PinnedEngine` with the hash of their own fixture
//! instead, which keeps the production trust anchor override-free.

#![allow(dead_code)]

include!("../build_support.rs");

use std::sync::{Mutex, MutexGuard, OnceLock};

/// `NEEDLE_NO_DOWNLOAD` and `NEEDLE_ENGINE_CACHE_DIR` are process-global, and
/// `cargo test` runs tests in threads. Every test that touches one takes this
/// lock. (`#[serial_test::serial]` would need the dependency in a crate whose
/// whole point is having almost none.)
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A `file://` URL for a directory, as `curl --output` accepts it.
fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

/// Lay out `<mirror>/<platform>/libneedle.a` holding `bytes`, and return a
/// `PinnedEngine` pinning that content for `target`.
fn mirror(root: &Path, target: &'static str, platform: &'static str, bytes: &[u8]) -> PinnedEngine {
    let dir = root.join(platform);
    std::fs::create_dir_all(&dir).expect("mirror dir");
    let file = dir.join("libneedle.a");
    std::fs::write(&file, bytes).expect("write artifact");
    let sha = sha256_file(&file).expect("hash fixture");
    PinnedEngine {
        target,
        platform,
        // `PinnedEngine` holds `&'static str`, matching the production table;
        // a test fixture's hash is only known at runtime, hence the leak. It
        // is one small string per test process.
        sha256: Box::leak(sha.into_boxed_str()),
        bytes: bytes.len() as u64,
    }
}

/// The happy path: not cached → downloaded → verified → cached under its own
/// checksum, and the returned path is what the linker will be pointed at.
#[test]
fn a_pinned_engine_is_downloaded_verified_and_cached() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = mirror(
        tmp.path(),
        "aarch64-apple-darwin",
        "macos-arm64",
        b"pretend this is an ar archive",
    );
    let cache = tmp.path().join("cache");

    let path = ensure_cached_engine(&engine, &file_url(tmp.path()), &cache)
        .unwrap_or_else(|e| panic!("{}", e.message(engine.target)));

    assert_eq!(path, cache.join(engine.sha256).join("libneedle.a"));
    assert_eq!(
        sha256_file(&path).expect("hash result"),
        engine.sha256,
        "the cached engine must be the pinned bytes"
    );
    assert!(
        !std::fs::read_dir(path.parent().expect("parent"))
            .expect("read cache dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".part")),
        "no partial download may survive a successful fetch"
    );
}

/// A cache hit must not re-download. Proven by deleting the mirror: the second
/// call has nowhere to fetch from, so if it succeeds it can only have used the
/// cache.
#[test]
fn a_cached_engine_is_reused_without_downloading_again() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = mirror(
        tmp.path(),
        "aarch64-unknown-linux-gnu",
        "linux-arm64",
        b"engine bytes",
    );
    let cache = tmp.path().join("cache");
    let base = file_url(tmp.path());

    let first = ensure_cached_engine(&engine, &base, &cache).expect("first fetch");
    std::fs::remove_dir_all(tmp.path().join(engine.platform)).expect("remove mirror");

    let second =
        ensure_cached_engine(&engine, &base, &cache).expect("a cache hit must not need the mirror");
    assert_eq!(first, second);
}

/// Bytes that do not match the pin are discarded, not linked — and the error
/// says both hashes so a reader can tell tampering from a stale pin.
#[test]
fn a_checksum_mismatch_discards_the_download_and_reports_both_hashes() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut engine = mirror(
        tmp.path(),
        "aarch64-unknown-linux-gnu",
        "linux-arm64",
        b"the real engine",
    );
    let honest = engine.sha256;
    // Pin something the mirror does not serve: the mirror is now hostile.
    engine.sha256 = "0000000000000000000000000000000000000000000000000000000000000000";
    let cache = tmp.path().join("cache");

    let err = ensure_cached_engine(&engine, &file_url(tmp.path()), &cache)
        .expect_err("a mismatched engine must not be accepted");
    let message = err.message(engine.target);
    assert!(message.contains(engine.sha256), "expected pin: {message}");
    assert!(message.contains(honest), "actual hash: {message}");
    assert!(
        !cache.join(engine.sha256).join("libneedle.a").exists(),
        "the rejected bytes must not be cached"
    );
    assert!(
        std::fs::read_dir(cache.join(engine.sha256))
            .expect("read cache dir")
            .filter_map(Result::ok)
            .next()
            .is_none(),
        "no `.part` leftover may survive a rejected fetch"
    );
}

/// A cache entry that has been truncated or tampered with must be refetched,
/// not linked. (Content addressing makes a stale entry impossible; corruption
/// is still possible, so the hit path re-hashes.)
#[test]
fn a_corrupt_cache_entry_is_refetched_rather_than_trusted() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = mirror(
        tmp.path(),
        "aarch64-apple-darwin",
        "macos-arm64",
        b"good engine bytes",
    );
    let cache = tmp.path().join("cache");
    let base = file_url(tmp.path());

    let path = ensure_cached_engine(&engine, &base, &cache).expect("first fetch");
    std::fs::write(&path, b"truncated").expect("corrupt the cache");

    let again = ensure_cached_engine(&engine, &base, &cache).expect("refetch");
    assert_eq!(again, path);
    assert_eq!(
        sha256_file(&path).expect("hash"),
        engine.sha256,
        "the corrupt entry must have been replaced by the pinned bytes"
    );
}

/// `NEEDLE_NO_DOWNLOAD=1` is the offline/air-gapped opt-out: no transfer is
/// attempted at all, and the message names the alternatives.
#[test]
fn no_download_opts_out_of_the_network_entirely() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = mirror(
        tmp.path(),
        "aarch64-apple-darwin",
        "macos-arm64",
        b"engine bytes",
    );
    let cache = tmp.path().join("cache");

    // SAFETY: mutating the environment is sound here because `env_lock`
    // serializes every test in this file that reads or writes these vars.
    unsafe { std::env::set_var(NO_DOWNLOAD_ENV, "1") };
    let result = ensure_cached_engine(&engine, &file_url(tmp.path()), &cache);
    unsafe { std::env::remove_var(NO_DOWNLOAD_ENV) };

    let err = result.expect_err("the opt-out must stop the fetch");
    let message = err.message(engine.target);
    assert!(message.contains(NO_DOWNLOAD_ENV), "{message}");
    assert!(message.contains("vendor/"), "{message}");
    assert!(
        message.contains("needle-ffi"),
        "the message must name the feature to drop: {message}"
    );
    assert!(
        !cache.exists(),
        "the opt-out must not even create the cache dir"
    );
}

/// An opted-out build still uses an already-cached engine: the flag forbids
/// downloading, not linking.
#[test]
fn no_download_still_uses_an_engine_that_is_already_cached() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = mirror(
        tmp.path(),
        "aarch64-apple-darwin",
        "macos-arm64",
        b"engine bytes",
    );
    let cache = tmp.path().join("cache");
    let base = file_url(tmp.path());
    let path = ensure_cached_engine(&engine, &base, &cache).expect("prime the cache");

    // SAFETY: see `no_download_opts_out_of_the_network_entirely`.
    unsafe { std::env::set_var(NO_DOWNLOAD_ENV, "1") };
    let result = ensure_cached_engine(&engine, &base, &cache);
    unsafe { std::env::remove_var(NO_DOWNLOAD_ENV) };

    assert_eq!(result.ok(), Some(path));
}

/// An unreachable mirror is a failure with a remedy, not a panic and not a
/// silent success. `file://` to a path that does not exist is the hermetic
/// stand-in for "this machine is offline".
#[test]
fn an_unreachable_mirror_fails_with_an_actionable_message() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = mirror(
        tmp.path(),
        "aarch64-apple-darwin",
        "macos-arm64",
        b"engine bytes",
    );
    let cache = tmp.path().join("cache");

    let err = ensure_cached_engine(&engine, &file_url(&tmp.path().join("gone")), &cache)
        .expect_err("an unreachable mirror must fail");
    let message = err.message(engine.target);
    assert!(
        message.contains("could not download"),
        "must name what failed: {message}"
    );
    assert!(
        message.contains("needle-ffi"),
        "must name the one-command way out: {message}"
    );
    assert!(
        message.contains(NO_DOWNLOAD_ENV),
        "must name the offline opt-out: {message}"
    );
    assert!(
        !cache.join(engine.sha256).join("libneedle.a").exists(),
        "a failed transfer must leave nothing behind"
    );
}

/// The pinned table only claims targets whose checksum was actually verified,
/// and every entry is well-formed. A guessed or malformed pin is a
/// supply-chain hole, so this asserts the shape rather than the values.
#[test]
fn every_pinned_engine_is_well_formed_and_unique() {
    assert!(!PINNED_ENGINES.is_empty());
    for engine in PINNED_ENGINES {
        assert_eq!(
            engine.sha256.len(),
            64,
            "{}: a SHA-256 is 64 hex chars",
            engine.target
        );
        assert!(
            engine.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "{}: non-hex in the pin",
            engine.target
        );
        assert!(
            engine.sha256.chars().all(|c| !c.is_ascii_uppercase()),
            "{}: pins are lowercase hex (that is what `sha256_file` produces)",
            engine.target
        );
        assert!(engine.bytes > 0, "{}: zero-byte pin", engine.target);
        assert!(!engine.platform.is_empty());
    }
    for (i, a) in PINNED_ENGINES.iter().enumerate() {
        for b in &PINNED_ENGINES[i + 1..] {
            assert_ne!(a.target, b.target, "duplicate target in the pinned table");
        }
    }
}

/// Intel macOS has no published engine, so it must not be claimed — this is
/// the concrete reason `needle-ffi` cannot be a default feature, and a
/// regression here would turn a graceful "static routing" build into a link
/// error for every Intel Mac.
#[test]
fn unsupported_targets_are_reported_as_unsupported_not_guessed() {
    assert!(pinned_engine("x86_64-apple-darwin").is_none());
    assert!(pinned_engine("x86_64-unknown-linux-musl").is_none());

    let message = FetchError::UnsupportedTarget.message("x86_64-apple-darwin");
    assert!(message.contains("x86_64-apple-darwin"), "{message}");
    assert!(
        message.contains("static rules"),
        "must say forge still works: {message}"
    );
    for engine in PINNED_ENGINES {
        assert!(
            message.contains(engine.target),
            "must list what *is* supported: {message}"
        );
    }
}

/// x86_64 is deliberately absent, and this is the test that keeps it that way.
///
/// Both x86_64 archives (`linux-x86_64`, `windows-x86_64`) leave
/// `std::__1::__hash_memory` undefined, and no Debian/Ubuntu libc++ package
/// defines it at any version — the symbol lives only inside Cactus's own libc++
/// build, which they ship pre-linked inside their `.so` and do not publish
/// separately. Pinning `x86_64-unknown-linux-gnu` would therefore promise a
/// download that cannot be linked, which is worse than admitting there is no
/// engine: the user would get a link error instead of a working static-routing
/// build plus an honest message.
#[test]
fn x86_64_linux_is_not_pinned_because_its_archive_cannot_be_linked() {
    assert!(
        pinned_engine("x86_64-unknown-linux-gnu").is_none(),
        "see PINNED_ENGINES: the x86_64 archive needs a libc++ nobody distributes"
    );
    let message = FetchError::UnsupportedTarget.message("x86_64-unknown-linux-gnu");
    assert!(message.contains("static rules"), "{message}");
    assert!(
        message.contains("aarch64-unknown-linux-gnu"),
        "must point at the Linux target that does work: {message}"
    );
}

/// The linker's filename expectation differs from the published filename on
/// Windows; the cache must be written under the name `-lneedle` resolves.
#[test]
fn the_cached_file_is_named_for_the_linker_not_the_download() {
    assert_eq!(lib_file_name("x86_64-pc-windows-msvc"), "needle.lib");
    assert_eq!(lib_file_name("aarch64-apple-darwin"), "libneedle.a");
    assert_eq!(lib_file_name("x86_64-unknown-linux-gnu"), "libneedle.a");
}

/// The opt-out reads as a flag, not as a string: `0`/`false`/empty are off.
#[test]
fn env_flags_treat_zero_false_and_empty_as_off() {
    let _guard = env_lock();
    const KEY: &str = "NEEDLE_TEST_FLAG_SHAPE";
    for (value, expected) in [
        ("1", true),
        ("true", true),
        ("yes", true),
        ("anything", true),
        ("0", false),
        ("false", false),
        ("FALSE", false),
        ("no", false),
        ("", false),
        ("   ", false),
    ] {
        // SAFETY: see `no_download_opts_out_of_the_network_entirely`.
        unsafe { std::env::set_var(KEY, value) };
        assert_eq!(env_flag(KEY), expected, "value {value:?}");
    }
    unsafe { std::env::remove_var(KEY) };
    assert!(!env_flag(KEY), "an unset flag is off");
}

/// A machine with both static libc++ archives installed, which is what the
/// CI and release images have (`libc++-dev` + `libc++abi-dev`).
fn libcxx_installed() -> CxxAvailability {
    CxxAvailability {
        static_cxx: Some(PathBuf::from("/usr/lib/x86_64-linux-gnu")),
        static_cxxabi: Some(PathBuf::from("/usr/lib/x86_64-linux-gnu")),
        shared_cxx: true,
    }
}

/// The regression that broke the first brain-enabled Linux build: `build.rs`
/// emitted `-lstdc++` on Linux because that is the *platform* default, but the
/// published engine is clang/libc++ on every platform, so the link failed on
/// `std::__1::basic_string<…>::append`. `std::__1` is libc++'s inline
/// namespace; nothing on a Linux target may ever name libstdc++ by default
/// again.
///
/// Evidence for the rule is on `cxx_runtime_flags` (an `nm --undefined-only`
/// census of all five downloaded artifacts: libc++ symbols everywhere, zero
/// libstdc++ ones) and was confirmed by linking and running the real engine in
/// an `ubuntu:24.04` container.
#[test]
fn linux_links_libcxx_never_libstdcxx() {
    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
    ] {
        let flags = cxx_plan(target, None, &libcxx_installed()).flags;
        assert!(
            !flags.iter().any(|f| f.contains("stdc++")),
            "{target} must not name libstdc++: {flags:?}"
        );
        assert!(
            flags.contains(&"static=c++".to_string()),
            "{target} must link libc++: {flags:?}"
        );
        assert!(
            flags.contains(&"static=c++abi".to_string()),
            "{target} must link libc++abi (separate library on Linux): {flags:?}"
        );
        assert!(
            flags.contains(&"dylib=m".to_string()),
            "{target} must link libm — the engine calls powf/sincosf/expf: {flags:?}"
        );
    }
}

/// Static on Linux, not dynamic: dynamic libc++ links and runs but adds
/// `libc++.so.1`, `libc++abi.so.1` and `libunwind.so.1` to the binary's
/// runtime dependencies, which a normal distro does not ship — a downloaded
/// release asset would fail to start. Verified in a container both ways.
#[test]
fn linux_links_the_cxx_runtime_statically_so_assets_stay_portable() {
    let flags = cxx_plan("x86_64-unknown-linux-gnu", None, &libcxx_installed()).flags;
    for name in ["c++", "c++abi"] {
        assert!(
            flags.contains(&format!("static={name}")),
            "{name} must be static, not dylib: {flags:?}"
        );
        assert!(
            !flags.contains(&format!("dylib={name}")),
            "{name} must not be dynamic by default: {flags:?}"
        );
    }
}

/// macOS keeps the dynamic system libc++ (`libc++.1.dylib` is part of the OS,
/// and no static libc++ ships with the toolchain). This was the one target the
/// old OS-based guess got right, and it must stay right.
#[test]
fn apple_links_the_system_libcxx_dynamically() {
    for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let plan = cxx_plan(target, None, &CxxAvailability::default());
        assert_eq!(plan.flags, vec!["dylib=c++".to_string()]);
        assert!(plan.search_dirs.is_empty(), "{target}: {plan:?}");
        assert!(plan.warning.is_none(), "{target}: {plan:?}");
    }
}

/// The override exists for distro packagers, who must link the shared system
/// runtime rather than bundling a copy.
#[test]
fn the_cxx_runtime_is_overridable_for_packagers() {
    let target = "x86_64-unknown-linux-gnu";
    let have = libcxx_installed();

    // A packager linking the shared system runtime: dynamic, and no warning,
    // because they asked for it.
    let dynamic = cxx_plan(target, Some("libc++"), &have);
    assert_eq!(
        dynamic.flags,
        vec![
            "dylib=c++".to_string(),
            "dylib=c++abi".to_string(),
            "dylib=m".to_string(),
        ]
    );
    assert!(dynamic.warning.is_none(), "{dynamic:?}");

    // The escape hatch, for an engine somebody rebuilt against libstdc++.
    assert_eq!(
        cxx_plan(target, Some("libstdc++"), &have).flags,
        vec!["dylib=stdc++".to_string()]
    );

    // An unknown spelling must not silently become libstdc++ — it falls back to
    // the verified default for the target.
    assert_eq!(
        cxx_plan(target, Some("gnu"), &have).flags,
        cxx_plan(target, None, &have).flags
    );

    // "I have handled it myself": no flags, no search dirs, no warning. Used by
    // an engine that already carries its runtime — and by `build_notes.rs`'s
    // vendored-engine fixture, so a test about engine resolution does not need
    // a C++ runtime installed to run.
    let none = cxx_plan(target, Some("none"), &have);
    assert!(none.flags.is_empty(), "{none:?}");
    assert!(none.search_dirs.is_empty(), "{none:?}");
    assert!(none.warning.is_none(), "{none:?}");
    // Including on a machine with nothing installed — it must not fall through
    // to the warn-and-ask-for-static branch.
    let none_bare = cxx_plan(target, Some("none"), &CxxAvailability::default());
    assert!(none_bare.flags.is_empty(), "{none_bare:?}");
    assert!(none_bare.warning.is_none(), "{none_bare:?}");
    // And on macOS, where the default is dynamic libc++.
    assert!(
        cxx_plan("aarch64-apple-darwin", Some("none"), &have)
            .flags
            .is_empty()
    );

    assert!(known_cxx_runtime("none"));
    assert!(known_cxx_runtime("static-libc++"));
    assert!(known_cxx_runtime("libc++"));
    assert!(known_cxx_runtime("libstdc++"));
    assert!(!known_cxx_runtime("libc++-dev"));
    assert!(!known_cxx_runtime("gnu"));
    assert!(known_cxx_runtime("  LibC++  "), "trimmed, case-insensitive");
}

/// No static archives but a shared libc++ present: link dynamically rather than
/// failing, and say plainly that the binary now has runtime dependencies — a
/// developer building for themselves should not be blocked, but nobody should
/// cut a release asset this way without being told.
#[test]
fn a_machine_without_static_libcxx_links_dynamically_and_says_so() {
    let have = CxxAvailability {
        static_cxx: None,
        static_cxxabi: None,
        shared_cxx: true,
    };
    let plan = cxx_plan("x86_64-unknown-linux-gnu", None, &have);

    assert_eq!(
        plan.flags,
        vec![
            "dylib=c++".to_string(),
            "dylib=c++abi".to_string(),
            "dylib=m".to_string(),
        ]
    );
    let warning = plan.warning.expect("this must not happen silently");
    assert!(warning.contains("libc++-dev"), "{warning}");
    assert!(warning.contains("libc++abi-dev"), "{warning}");
    assert!(
        warning.contains("dynamically"),
        "must say what it did: {warning}"
    );
}

/// No libc++ at all: still ask for static, so the failure is the linker's
/// specific "cannot find -lc++" — with the package names already on screen —
/// rather than a silent fallback to a runtime the engine cannot use.
#[test]
fn a_machine_with_no_libcxx_names_the_package_to_install() {
    let plan = cxx_plan(
        "x86_64-unknown-linux-gnu",
        None,
        &CxxAvailability::default(),
    );

    assert!(plan.flags.contains(&"static=c++".to_string()), "{plan:?}");
    assert!(
        !plan.flags.iter().any(|f| f.contains("stdc++")),
        "never silently fall back to libstdc++: {plan:?}"
    );
    let warning = plan.warning.expect("this must not happen silently");
    assert!(warning.contains("libc++-dev"), "{warning}");
    assert!(warning.contains("libcxx-devel"), "Fedora too: {warning}");
}

/// `rustc`'s `static=` kind does its own lookup and does **not** inherit the C
/// compiler's search path, so a static plan is useless without the `-L` that
/// points at the archives. This is the second half of the CI fix: emitting
/// `static=c++` alone still failed with "could not find native static library
/// `c++`" on a machine where `cc -l:libc++.a` linked fine.
#[test]
fn a_static_plan_always_carries_the_search_dirs_rustc_needs() {
    let one_dir = cxx_plan("x86_64-unknown-linux-gnu", None, &libcxx_installed());
    assert_eq!(
        one_dir.search_dirs,
        vec![PathBuf::from("/usr/lib/x86_64-linux-gnu")],
        "one directory holding both archives is emitted once"
    );

    let split = cxx_plan(
        "x86_64-unknown-linux-gnu",
        None,
        &CxxAvailability {
            static_cxx: Some(PathBuf::from("/usr/lib/llvm-18/lib")),
            static_cxxabi: Some(PathBuf::from("/usr/lib/x86_64-linux-gnu")),
            shared_cxx: true,
        },
    );
    assert_eq!(
        split.search_dirs,
        vec![
            PathBuf::from("/usr/lib/llvm-18/lib"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        ],
        "archives in different directories both get a -L"
    );

    // A dynamic plan needs none: the linker's default path finds the .so.
    assert!(
        cxx_plan(
            "x86_64-unknown-linux-gnu",
            Some("libc++"),
            &libcxx_installed()
        )
        .search_dirs
        .is_empty()
    );
}

/// The directory-scan fallback exists because `cc -print-file-name=libc++abi.a`
/// comes up empty on Ubuntu 24.04 (the archive only ships under
/// `/usr/lib/llvm-<N>/lib`, which gcc never searches), so the multiarch name it
/// derives has to be right or the scan looks in the wrong place.
#[test]
fn the_multiarch_directory_name_is_derived_correctly() {
    assert_eq!(
        gnu_multiarch_triple("x86_64-unknown-linux-gnu").as_deref(),
        Some("x86_64-linux-gnu")
    );
    assert_eq!(
        gnu_multiarch_triple("aarch64-unknown-linux-gnu").as_deref(),
        Some("aarch64-linux-gnu")
    );
    assert_eq!(
        gnu_multiarch_triple("x86_64-unknown-linux-musl").as_deref(),
        Some("x86_64-linux-musl")
    );
    // Not Linux, not a multiarch layout.
    assert_eq!(gnu_multiarch_triple("aarch64-apple-darwin"), None);
    assert_eq!(gnu_multiarch_triple("x86_64-pc-windows-msvc"), None);
    // Malformed input must not panic.
    assert_eq!(gnu_multiarch_triple("weird"), None);
    assert_eq!(gnu_multiarch_triple(""), None);
}

/// The scan must not be fooled into reporting a directory that does not hold
/// the file, and must tolerate the conventional directories being absent.
#[test]
fn the_directory_scan_only_reports_a_real_hit() {
    assert_eq!(
        scan_lib_dirs(
            "libdefinitely-not-a-real-library.a",
            "x86_64-unknown-linux-gnu"
        ),
        None
    );
}

/// A cross build cannot trust `cc -print-file-name` (it answers for the host),
/// so it must not pretend to know what the target machine has.
#[test]
fn a_cross_build_probes_nothing() {
    assert_eq!(
        probe_cxx("aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"),
        CxxAvailability::default()
    );
    // Apple and MSVC never need the probe even natively.
    assert_eq!(
        probe_cxx("aarch64-apple-darwin", "aarch64-apple-darwin"),
        CxxAvailability::default()
    );
}

/// `NEEDLE_ENGINE_CACHE_DIR` wins over `CARGO_HOME`, so a build can be
/// pointed at a shared or read-only-primed cache.
#[test]
fn the_cache_dir_is_overridable() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    let explicit = tmp.path().join("explicit");

    // SAFETY: see `no_download_opts_out_of_the_network_entirely`.
    unsafe { std::env::set_var(CACHE_DIR_ENV, &explicit) };
    let chosen = engine_cache_root(&out_dir);
    unsafe { std::env::remove_var(CACHE_DIR_ENV) };
    assert_eq!(chosen, explicit);

    // Without the override it must be a stable, shared location — never
    // OUT_DIR, which a `cargo clean` wipes and which is per-crate-build.
    let fallback = engine_cache_root(&out_dir);
    assert!(
        fallback.ends_with("needle-engine"),
        "unexpected default cache root: {}",
        fallback.display()
    );
    assert_ne!(fallback, out_dir);
}
