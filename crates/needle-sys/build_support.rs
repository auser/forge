// Engine resolution: the logic behind `build.rs`, in a file both `build.rs`
// and `tests/engine_resolution.rs` pull in with `include!`. (Plain `//`
// comments, not `//!`: an included file is not a crate root.)
//
// Why `include!` and not a module: a build script cannot depend on the crate
// it builds, and `cargo test` never compiles `build.rs` at all — so without
// this split the download/verify/cache path could only be tested by shelling
// out to `cargo build` (slow, and unable to fake a checksum honestly). Shared
// by inclusion, the same code gets real unit tests against a `file://` mirror
// with no network and no env-var escape hatch for the checksum.
//
// Everything here is `#[allow(dead_code)]` at the include site: each consumer
// uses a different subset.

use std::path::{Path, PathBuf};
use std::process::Command;

/// An engine artifact whose SHA-256 forge has verified for a specific Rust
/// target. `platform` is the folder name inside the Hugging Face repo.
///
/// **Only verified targets belong here.** A target absent from this table
/// resolves to "no pinned engine", which is an honest, actionable outcome;
/// a target present with a guessed checksum would be a silent supply-chain
/// hole. See the table's own comment for the verification transcript.
struct PinnedEngine {
    target: &'static str,
    platform: &'static str,
    sha256: &'static str,
    bytes: u64,
}

/// Engines forge will download and link automatically.
///
/// **Verified 2026-09-24** (this table's provenance): each artifact was
/// downloaded from `DEFAULT_BASE_URL` and hashed locally with `shasum -a 256`;
/// every hash also matched the `x-linked-etag` Hugging Face serves for the
/// file, and macos-arm64 matched the value independently recorded in the
/// design spec's §8 in an earlier session.
///
/// Artifacts published in the same repo whose checksums were collected but
/// which are deliberately **not** wired up (see the report / spec §8):
///
/// ```text
/// linux-x86_64    2581e7d46acd4f66c5839bcfb06b0af11c157c8775636875beb0af5ca35ded54  1675104
/// windows-x86_64  6fb0b9bccfa9f54d46e05a279273c15021570a53a8b3945613d80d299ca1f634  1808664
/// windows-arm64   3a945065225cb383cab9b75333ebe0195d25c7e7c815f032d47857b354056d75  1650954
/// linux-armv7     b1c3cf3ac526cb01314529da2094b8e5b38f41acd5b4a956fc05f22fb4b99346  1334534
/// linux-riscv64   11e0eea3d8dff6826171a702f6e741c3cbedde4e42a1ca1959d3712092adbc53  1548596
/// ```
///
/// **The x86_64 archives cannot be linked against any distributed libc++** —
/// measured, and the reason `linux-x86_64` is on that list despite being the
/// most common Linux target. Both `linux-x86_64` and `windows-x86_64` leave
/// `std::__1::__hash_memory(void const*, unsigned long)`
/// (`_ZNSt3__113__hash_memoryEPKvm`) undefined, and **no** Debian/Ubuntu
/// libc++ package defines it — checked across libc++ 18 and 20, dev and
/// runtime, static archive and shared object, all zero. The symbol exists only
/// inside Cactus's own libc++ build, which they do not publish standalone:
/// their manylinux x86_64 wheel ships `libneedle3.so` with that runtime
/// *already linked in* (`objdump -p` shows only libm/libc/libpthread/libdl and
/// no undefined `__hash_memory`). So the `.a` for x86_64 is effectively
/// incomplete for external linking, while the arm64 `.a` links cleanly against
/// a stock libc++. Nothing forge can do about that from here — reconsider if
/// Cactus ships an x86_64 archive that links against a stock libc++, or if
/// forge ever links their self-contained `.so` instead of the `.a`.
///
/// Windows is held back for three independent reasons: the published file is
/// `libneedle.a` (an `ar` archive of a COFF `needle.cpp.obj`), not the
/// `needle.lib` an MSVC `-lneedle` resolves; it needs libc++, which an MSVC
/// toolchain does not provide at all; and the x86_64 one has the
/// `__hash_memory` problem above. armv7/riscv64 are held back because no forge
/// CI or release target builds them, so the link has never been exercised.
///
/// Note what is *not* published at all: there is no `macos-x86_64` folder in
/// the repo (checked against the HF `siblings` listing), so
/// `x86_64-apple-darwin` has no engine to fetch — which is one of the reasons
/// `needle-ffi` is not a default feature. See README's "Embedded Needle
/// brain".
///
/// What is left is arm64, on both platforms — and both are verified end to end
/// (real `decide` against real weights, statically linked, no new runtime
/// dependencies).
const PINNED_ENGINES: &[PinnedEngine] = &[
    PinnedEngine {
        target: "aarch64-apple-darwin",
        platform: "macos-arm64",
        sha256: "60cc14f1a2eda8da72b75f8f228fb72cadc2850b38702370f43e9660b74e951a",
        bytes: 1_158_184,
    },
    PinnedEngine {
        target: "aarch64-unknown-linux-gnu",
        platform: "linux-arm64",
        sha256: "b36c214437b5230bae89291f684de571dceb0922834a09ceeb09a8e21464a481",
        bytes: 1_539_978,
    },
];

/// Resolve-URL prefix for the pinned Hugging Face repo. Mirrors
/// `forge-needle`'s `weights.rs` constant of the same shape — the engine and
/// the weights come from the same Apache-2.0 repo.
const DEFAULT_BASE_URL: &str = "https://huggingface.co/Cactus-Compute/needle3/resolve/main";

/// Where an operator can point this build at an engine they fetched
/// themselves. Takes precedence over everything else.
const LIB_DIR_ENV: &str = "NEEDLE_LIB_DIR";
/// Set (to anything but `0`/`false`/empty) to forbid the build-time download:
/// offline machines, air-gapped builds, distro packaging. Resolution then
/// stops after `NEEDLE_LIB_DIR` and `vendor/`.
const NO_DOWNLOAD_ENV: &str = "NEEDLE_NO_DOWNLOAD";
/// Set to make an unresolvable engine a hard build failure instead of a
/// warning + engine-less build. Release and CI builds that intend to ship a
/// brain set this, so a brain-less binary can never be published under a
/// brain-enabled label.
const REQUIRE_ENV: &str = "NEEDLE_REQUIRE_ENGINE";
/// Mirror override for the download base URL (an internal artifact cache, or
/// a `file://` directory). The checksum is *not* overridable: a mirror has to
/// serve the same bytes.
const BASE_URL_ENV: &str = "NEEDLE_ENGINE_BASE_URL";
/// Override the shared engine cache directory. Default:
/// `$CARGO_HOME/needle-engine`, else `~/.cargo/needle-engine`, else `OUT_DIR`.
const CACHE_DIR_ENV: &str = "NEEDLE_ENGINE_CACHE_DIR";
/// Cargo sets `CARGO_FEATURE_<NAME>` for each feature enabled on *this*
/// crate. `fetch` is turned on by `forge-needle`'s `ffi` feature and by
/// nothing else, which is how a build script — which cannot see downstream
/// features — knows the engine is actually going to be linked.
const FETCH_FEATURE_ENV: &str = "CARGO_FEATURE_FETCH";

/// Every env var that changes this build script's decision, so cargo reruns
/// it when one of them changes.
const WATCHED_ENVS: &[&str] = &[
    LIB_DIR_ENV,
    NO_DOWNLOAD_ENV,
    REQUIRE_ENV,
    BASE_URL_ENV,
    CACHE_DIR_ENV,
    CXX_RUNTIME_ENV,
];

/// How long a single download attempt may take. The engine is 1.1-1.7 MB;
/// two minutes is generous even on a slow link, and bounded so a hung proxy
/// cannot wedge a build forever.
const DOWNLOAD_TIMEOUT_SECS: u32 = 120;

/// Why the engine could not be fetched. Each variant carries enough to build
/// a message that names the *one* thing the reader should do next — a build
/// script's diagnostic is often the only thing they will read.
#[derive(Debug)]
enum FetchError {
    /// No verified artifact is pinned for this target.
    UnsupportedTarget,
    /// `NEEDLE_NO_DOWNLOAD` is set.
    Disabled,
    /// The transfer itself failed (offline, proxy, 404, no `curl`/`wget`).
    Transfer(String),
    /// The bytes arrived but are not the bytes forge pinned.
    Checksum { expected: String, actual: String },
    /// Local filesystem trouble around the cache.
    Cache(String),
}

impl FetchError {
    /// One line, no jargon, ending in the single action that fixes it.
    fn message(&self, target: &str) -> String {
        match self {
            Self::UnsupportedTarget => format!(
                "no verified libneedle engine is published for {target}, so this build has no \
                 embedded brain. Either build without the feature (`cargo build` with no \
                 `--features needle-ffi` — forge routes with static rules and stays fully \
                 usable), or put an engine you trust in crates/needle-sys/vendor/{target}/ and \
                 point {LIB_DIR_ENV} at it. Verified targets: {}.",
                pinned_targets().join(", ")
            ),
            Self::Disabled => format!(
                "{NO_DOWNLOAD_ENV} is set, so the libneedle engine was not downloaded and this \
                 build has no embedded brain. Put an engine in \
                 crates/needle-sys/vendor/{target}/ (or point {LIB_DIR_ENV} at one), or build \
                 without `--features needle-ffi`."
            ),
            Self::Transfer(detail) => format!(
                "could not download the libneedle engine for {target} ({detail}). If this \
                 machine is offline, build without `--features needle-ffi` — forge routes with \
                 static rules and stays fully usable — or fetch the engine on a connected \
                 machine into crates/needle-sys/vendor/{target}/ and set {NO_DOWNLOAD_ENV}=1."
            ),
            Self::Checksum { expected, actual } => format!(
                "the downloaded libneedle engine for {target} does not match the checksum forge \
                 pins: expected {expected}, got {actual}. The download was discarded and \
                 nothing was linked. Retry; if it happens again, treat the mirror as \
                 untrustworthy and report it ({BASE_URL_ENV} selects a different one)."
            ),
            Self::Cache(detail) => format!(
                "could not write the libneedle engine cache for {target} ({detail}). Set \
                 {CACHE_DIR_ENV} to a writable directory, or vendor the engine into \
                 crates/needle-sys/vendor/{target}/."
            ),
        }
    }
}

/// The targets `PINNED_ENGINES` covers, for messages that should name them.
fn pinned_targets() -> Vec<&'static str> {
    PINNED_ENGINES.iter().map(|e| e.target).collect()
}

fn pinned_engine(target: &str) -> Option<&'static PinnedEngine> {
    PINNED_ENGINES.iter().find(|e| e.target == target)
}

/// `needle.lib` under MSVC, `libneedle.a` everywhere else — what `rustc`'s
/// `-lneedle` will look for.
fn lib_file_name(target: &str) -> String {
    if target.contains("msvc") {
        "needle.lib".to_string()
    } else {
        "libneedle.a".to_string()
    }
}

fn static_lib(dir: &Path, target: &str) -> Option<PathBuf> {
    let path = dir.join(lib_file_name(target));
    path.is_file().then_some(path)
}

/// Truthiness for the env flags: set and not obviously "off".
fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "no")
        }
        Err(_) => false,
    }
}

/// Lowercase hex SHA-256 of a file, streamed so a large artifact never has to
/// be held in memory.
fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Shared, content-addressed cache root for downloaded engines. Content
/// addressing means an entry is never stale (a new pin is a new directory)
/// and two concurrent builds cannot disagree about what a path holds.
///
/// `$CARGO_HOME/needle-engine` keeps it next to cargo's own caches, so the
/// usual "cache `~/.cargo`" CI step covers it; `OUT_DIR` is the last resort,
/// which only costs a redownload per clean build.
fn engine_cache_root(out_dir: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var(CACHE_DIR_ENV) {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(cargo_home) = std::env::var("CARGO_HOME") {
        let cargo_home = cargo_home.trim();
        if !cargo_home.is_empty() {
            return PathBuf::from(cargo_home).join("needle-engine");
        }
    }
    // `std::env::home_dir` is the pattern the rest of the repo uses for
    // `~/.cache/forge` and `~/.config/forge` (see forge-config, forge-needle's
    // weights.rs); un-deprecated since 1.85 and correct on every platform
    // forge targets.
    match std::env::home_dir() {
        Some(home) => home.join(".cargo").join("needle-engine"),
        None => out_dir.join("needle-engine"),
    }
}

/// Fetch `url` to `dest` using whatever transfer tool the machine has.
///
/// No HTTP crate on purpose: a build-time dependency on a TLS stack would be
/// compiled by every workspace build, and `curl` is present on macOS, on
/// essentially every Linux image, and as `curl.exe` in modern Windows. `wget`
/// and PowerShell are tried after it. `curl` also speaks `file://`, which is
/// what lets the tests exercise this path without a network.
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let mut attempts: Vec<String> = Vec::new();

    let curl = Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error"])
        .arg("--max-time")
        .arg(DOWNLOAD_TIMEOUT_SECS.to_string())
        .arg("--output")
        .arg(dest)
        .arg(url)
        .output();
    match curl {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => attempts.push(format!(
            "curl exited {}: {}",
            out.status.code().unwrap_or(-1),
            first_line(&String::from_utf8_lossy(&out.stderr))
        )),
        Err(e) => attempts.push(format!("curl unavailable: {e}")),
    }

    let wget = Command::new("wget")
        .arg("--quiet")
        .arg(format!("--timeout={DOWNLOAD_TIMEOUT_SECS}"))
        .arg("-O")
        .arg(dest)
        .arg(url)
        .output();
    match wget {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => attempts.push(format!(
            "wget exited {}: {}",
            out.status.code().unwrap_or(-1),
            first_line(&String::from_utf8_lossy(&out.stderr))
        )),
        Err(e) => attempts.push(format!("wget unavailable: {e}")),
    }

    if cfg!(windows) {
        // `-UseBasicParsing` keeps this working on images without IE's
        // engine; `$ProgressPreference` silences a progress bar that is
        // pointlessly slow when output is redirected.
        let script = format!(
            "$ProgressPreference='SilentlyContinue'; \
             Invoke-WebRequest -UseBasicParsing -TimeoutSec {DOWNLOAD_TIMEOUT_SECS} \
             -Uri '{url}' -OutFile '{}'",
            dest.display()
        );
        let ps = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output();
        match ps {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => attempts.push(format!(
                "powershell exited {}: {}",
                out.status.code().unwrap_or(-1),
                first_line(&String::from_utf8_lossy(&out.stderr))
            )),
            Err(e) => attempts.push(format!("powershell unavailable: {e}")),
        }
    }

    // A partial file from a failed transfer must never be hashed or cached.
    let _ = std::fs::remove_file(dest);
    Err(attempts.join("; "))
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string()
}

/// Return a verified engine for `engine.target`, downloading it into
/// `cache_root` only if it is not already cached.
///
/// The cache entry is keyed by the pinned checksum, so a hit is
/// self-describing: the file at `<cache_root>/<sha256>/<libname>` can only be
/// the bytes forge pinned. It is still re-hashed on a hit — cheap next to a
/// download, and the one thing that turns a truncated or tampered cache entry
/// into a refetch instead of a mystery link error.
///
/// Downloads land on a unique `.part` path and are renamed only after they
/// verify, so a concurrent build never observes a half-written engine and an
/// interrupted build leaves no poisoned cache entry.
fn ensure_cached_engine(
    engine: &PinnedEngine,
    base_url: &str,
    cache_root: &Path,
) -> Result<PathBuf, FetchError> {
    let lib_name = lib_file_name(engine.target);
    let dir = cache_root.join(engine.sha256);
    let final_path = dir.join(&lib_name);

    if final_path.is_file() {
        match sha256_file(&final_path) {
            Ok(actual) if actual == engine.sha256 => return Ok(final_path),
            // Corrupt or truncated cache entry: drop it and fetch again
            // rather than linking bytes nobody vouched for.
            _ => {
                let _ = std::fs::remove_file(&final_path);
            }
        }
    }

    if env_flag(NO_DOWNLOAD_ENV) {
        return Err(FetchError::Disabled);
    }

    std::fs::create_dir_all(&dir).map_err(|e| FetchError::Cache(format!("{}: {e}", dir.display())))?;

    let url = format!(
        "{}/{}/{}",
        base_url.trim_end_matches('/'),
        engine.platform,
        // The published filename is `libneedle.a` on every platform,
        // including Windows — `lib_name` is what the *linker* wants locally,
        // not what the repo serves.
        "libneedle.a"
    );
    let part = dir.join(format!(
        "{lib_name}.part.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));

    download(&url, &part).map_err(|e| FetchError::Transfer(format!("{url}: {e}")))?;

    let actual = match sha256_file(&part) {
        Ok(actual) => actual,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(FetchError::Cache(e));
        }
    };
    if actual != engine.sha256 {
        let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&part);
        return Err(FetchError::Checksum {
            expected: format!("{} ({} bytes)", engine.sha256, engine.bytes),
            actual: format!("{actual} ({size} bytes)"),
        });
    }

    if let Err(e) = std::fs::rename(&part, &final_path) {
        let _ = std::fs::remove_file(&part);
        // A concurrent build may have won the race and published the identical
        // bytes first (and on Windows, `rename` over an existing file fails
        // outright). The content-addressed path can only hold the pin, so a
        // destination that already verifies *is* success.
        let already_there = sha256_file(&final_path).is_ok_and(|actual| actual == engine.sha256);
        if !already_there {
            return Err(FetchError::Cache(format!(
                "{} -> {}: {e}",
                part.display(),
                final_path.display()
            )));
        }
    }
    Ok(final_path)
}

/// Step 3 of the resolution order: download-at-build with checksum
/// verification. Returns the *directory* to add to the link search path.
///
/// Never called unless the `fetch` feature is on — i.e. unless
/// `forge-needle/ffi` asked for a linkable engine.
fn fetch_engine_dir(target: &str, out_dir: &Path) -> Result<PathBuf, FetchError> {
    let engine = pinned_engine(target).ok_or(FetchError::UnsupportedTarget)?;
    let base_url = match std::env::var(BASE_URL_ENV) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_BASE_URL.to_string(),
    };
    let cached = ensure_cached_engine(engine, &base_url, &engine_cache_root(out_dir))?;
    match cached.parent() {
        Some(dir) => Ok(dir.to_path_buf()),
        None => Err(FetchError::Cache(format!(
            "{} has no parent directory",
            cached.display()
        ))),
    }
}

/// Collapse newlines: `cargo:warning=` is a single-line directive, so an
/// embedded newline would truncate the message at the first break.
fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Override which C++ standard library the engine is linked against. Values:
/// `static-libc++` (the default wherever the archives can be found), `libc++`
/// (dynamic — what a distro package build wants, so the system runtime is
/// shared), `libstdc++` (an escape hatch, correct only for an engine somebody
/// rebuilt against GNU libstdc++).
const CXX_RUNTIME_ENV: &str = "NEEDLE_CXX_RUNTIME";

/// How to link the C++ standard library the *engine artifact* needs.
///
/// **It is a property of the artifact, not of the operating system**, and
/// getting that backwards is what broke the first brain-enabled Linux build:
/// `build.rs` emitted `-lstdc++` on Linux because that is the platform
/// default, and the link failed on `std::__1::basic_string<…>::append`.
///
/// Measured, not assumed — `nm --undefined-only` over every published
/// `libneedle.a` (2026-09-24):
///
/// | artifact | undefined `_ZNSt3__1…` (libc++) | undefined `__cxx11` (libstdc++) |
/// | --- | --- | --- |
/// | `macos-arm64` | 44 | 0 |
/// | `linux-x86_64` | 45 | 0 |
/// | `linux-arm64` | 43 | 0 |
/// | `windows-x86_64` | 45 | 0 |
/// | `windows-arm64` | 43 | 0 |
///
/// `std::__1` is libc++'s inline namespace; `__cxx11` is libstdc++'s. Cactus
/// builds every platform with clang against libc++, so **libc++ is the answer
/// on every target** — macOS was only accidentally right before.
#[derive(Debug, PartialEq, Eq)]
struct CxxPlan {
    /// Extra `-L` directories. Needed because `rustc`'s `static=` kind does its
    /// own file lookup and does **not** inherit the C compiler's search path —
    /// which is why the first fix still failed with "could not find native
    /// static library `c++`" even on a machine where `cc -l:libc++.a` worked.
    search_dirs: Vec<PathBuf>,
    /// `cargo:rustc-link-lib=` values, in link order.
    flags: Vec<String>,
    /// A `cargo:warning` to emit, when the chosen plan is worth explaining.
    warning: Option<String>,
}

/// What the machine actually has, so [`cxx_plan`] can stay pure and testable.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CxxAvailability {
    /// Directory holding `libc++.a`, if one was found.
    static_cxx: Option<PathBuf>,
    /// Directory holding `libc++abi.a`, if one was found.
    static_cxxabi: Option<PathBuf>,
    /// Whether a shared `libc++` was found at all.
    shared_cxx: bool,
}

/// Decide the link plan. Pure: every impure input arrives as an argument, so
/// all the branches are unit-tested without needing a C compiler or a
/// particular distro.
///
/// Linux prefers **static** libc++. Verified both ways in an `ubuntu:24.04`
/// container: dynamic links and runs but adds `libc++.so.1`, `libc++abi.so.1`
/// and `libunwind.so.1` to the binary's runtime dependencies — three libraries
/// a normal distro does not ship, which would make a downloaded release asset
/// fail to start on the user's machine. Static leaves the runtime profile
/// exactly as it was before the brain existed (`libm`, `libgcc_s`, `libc`),
/// which is what lets Linux stay in the brain-enabled release set at all.
///
/// macOS links dynamically: `libc++.1.dylib` is part of the OS, and no static
/// libc++ ships with the toolchain.
fn cxx_plan(target: &str, requested: Option<&str>, have: &CxxAvailability) -> CxxPlan {
    let plain = |flags: &[&str]| CxxPlan {
        search_dirs: Vec::new(),
        flags: flags.iter().map(|s| (*s).to_string()).collect(),
        warning: None,
    };

    match requested {
        Some("libstdc++") => return plain(&["dylib=stdc++"]),
        Some("libc++") if target.contains("apple") => return plain(&["dylib=c++"]),
        Some("libc++") => return plain(&["dylib=c++", "dylib=c++abi", "dylib=m"]),
        // Unrecognised values fall through to the default; `build.rs` warns
        // about the spelling separately.
        _ => {}
    }

    if target.contains("apple") || target.contains("freebsd") {
        return plain(&["dylib=c++"]);
    }
    if target.contains("msvc") {
        // No MSVC target is wired up (see `PINNED_ENGINES`): the published
        // Windows engine needs libc++, which an MSVC toolchain does not provide
        // at all — a second, independent reason Windows is held back, beyond
        // the `libneedle.a`/`needle.lib` naming mismatch.
        return plain(&[]);
    }
    if target.contains("android") {
        // NDK naming. Untested — no Android target is wired up.
        return plain(&["static=c++_static", "static=c++abi"]);
    }

    // Linux and other ELF targets. `dylib=m` is explicit rather than relying on
    // std's link args: the engine calls `powf`/`sincosf`/`expf` directly (the
    // container probe failed on exactly those until `-lm` was added), and it
    // costs nothing if something else already named it.
    let elf_static = ["static=c++", "static=c++abi", "dylib=m"];
    let elf_dynamic = ["dylib=c++", "dylib=c++abi", "dylib=m"];
    let strings = |flags: &[&str; 3]| flags.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

    match (&have.static_cxx, &have.static_cxxabi) {
        (Some(cxx), Some(cxxabi)) => {
            let mut search_dirs = vec![cxx.clone()];
            if cxxabi != cxx {
                search_dirs.push(cxxabi.clone());
            }
            CxxPlan {
                search_dirs,
                flags: strings(&elf_static),
                warning: None,
            }
        }
        _ if have.shared_cxx => CxxPlan {
            search_dirs: Vec::new(),
            flags: strings(&elf_dynamic),
            warning: Some(format!(
                "libc++.a/libc++abi.a not found, so libc++ is linked dynamically — this binary \
                 will need libc++1 and libc++abi1 installed wherever it runs. Install \
                 libc++-dev and libc++abi-dev (Debian/Ubuntu) or libcxx-devel and \
                 libcxxabi-devel (Fedora) for a self-contained build, or set \
                 {CXX_RUNTIME_ENV}=libc++ to ask for this on purpose and silence this."
            )),
        },
        // Nothing found: still ask for static, so the failure is the linker's
        // specific "cannot find -lc++" rather than a confusing runtime crash,
        // and put the package names next to it.
        _ => CxxPlan {
            search_dirs: Vec::new(),
            flags: strings(&elf_static),
            warning: Some(format!(
                "no libc++ found on this machine, and the needle engine needs it (it is built \
                 with clang/libc++ on every platform, not libstdc++). Install libc++-dev and \
                 libc++abi-dev (Debian/Ubuntu) or libcxx-devel and libcxxabi-devel (Fedora); \
                 {CXX_RUNTIME_ENV} overrides the choice if your engine differs."
            )),
        },
    }
}

/// Is `value` a spelling [`cxx_plan`] understands? Used only to warn.
fn known_cxx_runtime(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "static-libc++" | "libc++" | "libstdc++"
    )
}

/// The `NEEDLE_CXX_RUNTIME` request, normalised, if one was made.
fn requested_cxx_runtime() -> Option<String> {
    std::env::var(CXX_RUNTIME_ENV)
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

/// Where a static C++ runtime archive lives, or `None`.
///
/// Two strategies, because on Ubuntu 24.04 neither alone is enough — measured:
/// `cc -print-file-name=libc++.a` resolves (the `libc++-dev` package puts it in
/// gcc's search path) but `cc -print-file-name=libc++abi.a` does **not**,
/// because `libc++abi-dev` only ships it under `/usr/lib/llvm-<N>/lib`, which
/// gcc never searches. Asking `clang` would find it, but needing clang
/// installed merely to *locate* a file is a bad prerequisite for a Rust build.
///
/// 1. Ask the C compiler (`-print-file-name`, understood by gcc and clang).
///    Authoritative when it answers, and adapts to non-standard toolchains.
/// 2. Otherwise scan the conventional directories, including versioned LLVM
///    ones. Mixing Ubuntu's `libc++.a` with llvm-18's `libc++abi.a` is fine —
///    they are the same LLVM build — and this was verified to link and run.
fn find_lib_dir(name: &str, target: &str) -> Option<PathBuf> {
    if let Some(dir) = ask_compiler(name) {
        return Some(dir);
    }
    scan_lib_dirs(name, target)
}

/// Strategy 1: `-print-file-name`. Returns the *directory*, and only when the
/// file really exists — gcc echoes the bare name back when it finds nothing.
fn ask_compiler(name: &str) -> Option<PathBuf> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(cc) = std::env::var("CC") {
        let cc = cc.trim();
        if !cc.is_empty() {
            candidates.push(cc.to_string());
        }
    }
    candidates.extend(["cc".to_string(), "clang".to_string(), "gcc".to_string()]);

    for compiler in candidates {
        let Ok(out) = Command::new(&compiler)
            .arg(format!("-print-file-name={name}"))
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if path.is_absolute() && path.is_file() {
            return path.parent().map(Path::to_path_buf);
        }
    }
    None
}

/// Strategy 2: look in the places distributions actually use.
///
/// `/usr/lib/llvm-*/lib` entries are sorted so the highest LLVM version wins,
/// which matches what a `clang` on the same machine would pick.
fn scan_lib_dirs(name: &str, target: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(triple) = gnu_multiarch_triple(target) {
        dirs.push(PathBuf::from(format!("/usr/lib/{triple}")));
        dirs.push(PathBuf::from(format!("/lib/{triple}")));
    }
    dirs.push(PathBuf::from("/usr/lib64"));
    dirs.push(PathBuf::from("/usr/local/lib"));
    dirs.push(PathBuf::from("/usr/lib"));

    let mut llvm: Vec<PathBuf> = std::fs::read_dir("/usr/lib")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("llvm-"))
        })
        .map(|p| p.join("lib"))
        .collect();
    // "llvm-9" before "llvm-18" lexically, so compare the version numerically.
    llvm.sort_by_key(|p| {
        p.parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("llvm-"))
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    });
    llvm.reverse();
    dirs.extend(llvm);

    dirs.into_iter().find(|dir| dir.join(name).is_file())
}

/// `x86_64-unknown-linux-gnu` → `x86_64-linux-gnu`, the Debian/Ubuntu
/// multiarch directory name. `None` for anything that is not a Linux triple of
/// that shape.
fn gnu_multiarch_triple(target: &str) -> Option<String> {
    let mut parts = target.split('-');
    let arch = parts.next()?;
    let _vendor = parts.next()?;
    let os = parts.next()?;
    if os != "linux" {
        return None;
    }
    let abi = parts.next().unwrap_or("gnu");
    Some(format!("{arch}-{os}-{abi}"))
}

/// Probe the machine for [`cxx_plan`]'s inputs.
///
/// Only meaningful for a native ELF build: `-print-file-name` answers for the
/// host toolchain, so a cross build gets an empty answer and falls back to the
/// warn-and-ask-for-static branch, where `CXX_RUNTIME_ENV` and a manual
/// `rustc-link-search` are the operator's tools. Apple and MSVC never need it.
fn probe_cxx(target: &str, host: &str) -> CxxAvailability {
    if target != host || target.contains("apple") || target.contains("msvc") {
        return CxxAvailability::default();
    }
    CxxAvailability {
        static_cxx: find_lib_dir("libc++.a", target),
        static_cxxabi: find_lib_dir("libc++abi.a", target),
        shared_cxx: find_lib_dir("libc++.so", target).is_some(),
    }
}
