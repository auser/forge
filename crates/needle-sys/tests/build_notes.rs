//! The build script's *user-visible* behavior, checked by actually running
//! cargo.
//!
//! Build scripts are separate crates that `cargo test` never covers, so
//! these tests drive real `cargo build` runs and read what a user reads.
//!
//! Each run builds a **standalone copy** of this crate — the real `build.rs`,
//! `src/lib.rs` and `needle.h`, with a minimal manifest and, crucially, *no*
//! `vendor/` directory. That is what makes the assertions mean something on
//! any machine: a developer who has fetched `libneedle.a` into
//! `crates/needle-sys/vendor/<target>/` (as `just verify-ffi` needs) would
//! otherwise never exercise the missing-library path at all, and the first
//! version of this test passed vacuously on exactly such a machine.
//!
//! Regression under test: `needle-sys` is a workspace member, so it builds
//! even when nothing links it and the `ffi` feature is off. It used to emit
//! `cargo:warning=needle-sys: no libneedle.a found…` there, which users read
//! as a build error on a plain `cargo build --release --locked`.
//!
//! The download path (resolution step 3) is unit-tested in
//! `engine_resolution.rs` instead; these runs only assert what cargo shows a
//! *user*, and every one of them sets `NEEDLE_NO_DOWNLOAD=1` so no test in
//! this file can reach the network.
//!
//! **Two environment assumptions these tests must never make again**, both
//! learned by having them pass locally and fail in CI:
//!
//! 1. *That the host is a pinned target.* Only arm64 macOS and arm64 Linux are
//!    in `PINNED_ENGINES`, so on x86_64 Linux the build script reports "no
//!    verified engine is published" and never reaches the download branch at
//!    all. Any test about download behaviour has to ask [`pinned_engine`] which
//!    situation it is in — hence the `include!` below, which shares the real
//!    table rather than duplicating it.
//! 2. *That a C++ runtime is installed.* Resolving an engine makes `build.rs`
//!    emit `cargo:rustc-link-lib=static=c++`, and `rustc` resolves `static=`
//!    libraries even for a lib-only build. The required `verify` CI job
//!    deliberately installs no C++ packages, so a fixture that resolves an
//!    engine must set `NEEDLE_CXX_RUNTIME=none` to keep runtime selection out
//!    of a test about engine resolution.

#![allow(dead_code)]

// The real pinned table and helpers, shared with `build.rs` itself. This also
// provides the `std::path`/`std::process` imports these tests use.
include!("../build_support.rs");

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// One shared target directory for every `cargo build` these tests run,
/// inside the workspace's own `target/`.
///
/// Per-test target dirs would recompile the `sha2` build-dependency (and its
/// five transitive crates) once per test — about a minute of `just verify` for
/// no benefit. Cargo locks a target directory, so concurrent test threads
/// serialize on it rather than corrupting it, and `cargo clean` still reaches
/// this because it lives under `target/`.
fn shared_target_dir() -> PathBuf {
    crate_dir()
        .join("..")
        .join("..")
        .join("target")
        .join("needle-sys-build-tests")
}

/// Copy the crate's real sources into `dir` with a self-contained manifest
/// and no vendored engine. Returns the manifest path.
fn standalone_copy(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("src dir");
    for (from, to) in [
        (crate_dir().join("build.rs"), dir.join("build.rs")),
        (
            crate_dir().join("build_support.rs"),
            dir.join("build_support.rs"),
        ),
        (crate_dir().join("needle.h"), dir.join("needle.h")),
        (crate_dir().join("src").join("lib.rs"), src.join("lib.rs")),
    ] {
        std::fs::copy(&from, &to).unwrap_or_else(|e| panic!("copy {}: {e}", from.display()));
    }
    let manifest = dir.join("Cargo.toml");
    // `[workspace]` makes this copy its own workspace root, so cargo does
    // not walk up and adopt whatever workspace the temp dir happens to sit
    // under. No dev-dependencies: these runs only `build`.
    std::fs::write(
        &manifest,
        "[workspace]\n\n\
         [package]\n\
         name = \"needle-sys\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         links = \"needle\"\n\
         \n[features]\n\
         fetch = []\n\
         \n[dependencies]\n\
         \n[build-dependencies]\n\
         sha2 = \"0.10\"\n",
    )
    .expect("write manifest");
    manifest
}

/// Build the standalone copy, with `NEEDLE_LIB_DIR` removed unless given.
/// Returns (success, stdout+stderr).
fn build(root: &Path, lib_dir: Option<&Path>) -> (bool, String) {
    build_with(root, lib_dir, &[], &[])
}

/// As [`build`], plus extra cargo arguments (`--features fetch`) and extra
/// environment variables.
///
/// `NEEDLE_NO_DOWNLOAD=1` is always set: these tests assert messages, and a
/// real download would make them depend on the network and on Hugging Face
/// being up. `NEEDLE_ENGINE_CACHE_DIR` points inside the temp dir so a
/// developer's primed `$CARGO_HOME/needle-engine` cannot turn the
/// engine-absent assertions vacuous. `NEEDLE_CXX_RUNTIME` is scrubbed so a
/// developer's shell cannot change what a fixture links.
fn build_with(
    root: &Path,
    lib_dir: Option<&Path>,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> (bool, String) {
    let manifest = standalone_copy(root);
    let mut cmd = Command::new(env!("CARGO"));
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(shared_target_dir())
        .args(extra)
        .env_remove("NEEDLE_LIB_DIR")
        .env_remove("NEEDLE_REQUIRE_ENGINE")
        .env_remove("NEEDLE_CXX_RUNTIME")
        .env("NEEDLE_NO_DOWNLOAD", "1")
        .env("NEEDLE_ENGINE_CACHE_DIR", root.join("engine-cache"))
        // Colour codes would make the string assertions brittle.
        .env("CARGO_TERM_COLOR", "never");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    if let Some(dir) = lib_dir {
        cmd.env("NEEDLE_LIB_DIR", dir);
    }
    let output = cmd.output().expect("cargo build runs");
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

/// A default build with no engine present must be completely silent about
/// it. This is the user-facing contract: `cargo build --release --locked`
/// on a fresh checkout prints no warnings.
#[test]
fn a_default_build_emits_no_warning_about_the_missing_engine() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (ok, output) = build(tmp.path(), None);
    assert!(ok, "needle-sys must build without the engine:\n{output}");
    assert!(
        !output.contains("warning:"),
        "a default build must not warn at all:\n{output}"
    );
    assert!(
        !output.to_lowercase().contains("libneedle"),
        "the missing-library note must not surface without -vv:\n{output}"
    );
}

/// The note still exists — it is just verbose-only, so someone debugging a
/// link failure can find it.
#[test]
fn the_missing_engine_note_is_still_there_under_verbose_output() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = standalone_copy(tmp.path());
    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("-vv")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(shared_target_dir())
        .env_remove("NEEDLE_LIB_DIR")
        .env("NEEDLE_NO_DOWNLOAD", "1")
        .env("NEEDLE_ENGINE_CACHE_DIR", tmp.path().join("engine-cache"))
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("cargo build -vv runs");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{combined}");
    assert!(
        combined.contains("needle-sys: no libneedle.a"),
        "the note should be readable under -vv:\n{combined}"
    );
    assert!(
        combined.contains("ffi"),
        "the note should say which feature actually needs the engine:\n{combined}"
    );
}

/// `NEEDLE_LIB_DIR` pointing somewhere without an engine is operator error:
/// it must fail, and say what is wrong, where it looked, and where to get
/// one.
#[test]
fn a_wrong_needle_lib_dir_fails_loudly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).expect("mkdir");

    let (ok, output) = build(&tmp.path().join("crate"), Some(&empty));
    assert!(!ok, "a wrong NEEDLE_LIB_DIR must fail the build:\n{output}");
    assert!(
        output.contains("NEEDLE_LIB_DIR is set to"),
        "the failure must name the variable and the directory:\n{output}"
    );
    assert!(
        output.contains("huggingface.co/Cactus-Compute/needle3"),
        "the failure must say where to get an engine:\n{output}"
    );
    assert!(
        output.contains("warning:"),
        "operator error should also surface as a cargo warning:\n{output}"
    );
}

/// With `fetch` on — i.e. somebody asked for `--features needle-ffi` — an
/// engine that cannot be resolved is a `cargo:warning`, not silence: the user
/// *did* ask for a brain, and the only symptom otherwise would be a link
/// error further down the build with no explanation attached.
///
/// It is a warning rather than a failure on purpose: `just lint-ffi` and CI's
/// link-free clippy pass compile the `ffi` code on machines with no engine,
/// and that coverage guarantee outranks pre-empting a link error the warning
/// already explains.
#[test]
fn with_fetch_enabled_an_unresolvable_engine_warns_with_the_one_remedy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (ok, output) = build_with(tmp.path(), None, &["--features", "fetch"], &[]);
    assert!(
        ok,
        "an unresolvable engine must not fail the build (lint-ffi depends on it):\n{output}"
    );
    assert!(
        output.contains("warning:") && output.contains("needle-sys:"),
        "the user asked for the engine, so its absence must be visible:\n{output}"
    );
    assert!(
        output.contains("needle-ffi"),
        "the warning must name the feature to drop for an engine-less build:\n{output}"
    );
}

/// `NEEDLE_REQUIRE_ENGINE=1` turns that warning into a hard stop. This is what
/// release/CI builds set so a brain-less binary can never ship labelled
/// brain-enabled — and the failure must read as a message, not as a build
/// script panic with a source location.
#[test]
fn require_engine_turns_an_unresolvable_engine_into_a_build_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = standalone_copy(tmp.path());
    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(shared_target_dir())
        .args(["--features", "fetch"])
        .env_remove("NEEDLE_LIB_DIR")
        .env_remove("NEEDLE_CXX_RUNTIME")
        .env("NEEDLE_NO_DOWNLOAD", "1")
        .env("NEEDLE_REQUIRE_ENGINE", "1")
        .env("NEEDLE_ENGINE_CACHE_DIR", tmp.path().join("engine-cache"))
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("cargo build runs");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "NEEDLE_REQUIRE_ENGINE must make this fatal:\n{combined}"
    );
    assert!(
        combined.contains("NEEDLE_REQUIRE_ENGINE is set, so this is fatal"),
        "the failure must say why it was fatal:\n{combined}"
    );
    assert!(
        !combined.contains("panicked at"),
        "the failure must read as a message, not a panic:\n{combined}"
    );
}

/// With `fetch` on and no engine available, nothing may touch the network and
/// no cache may appear — and the warning must name the *actual* reason.
///
/// Which reason that is depends on the host, and the assertion is deliberately
/// target-aware rather than forcing a pinned target into the fixture. Forcing
/// one would mean `cargo build --target <pinned triple>`, which needs that
/// target's std installed via rustup — an environment assumption at least as
/// fragile as the one that broke this test in the first place, and it would
/// stop exercising the path a real user on this machine actually takes.
///
/// So: on a pinned host the opt-out is what suppressed the fetch and the
/// message must say so; on an unpinned host resolution stops earlier, at "no
/// verified engine is published for this target", and claiming the opt-out did
/// it would be a lie. Both branches still assert the two things this test
/// exists for — no attempt, no cache — and the opt-out's own wording and
/// semantics are covered unconditionally, on every platform, by
/// `engine_resolution.rs`'s `no_download_opts_out_of_the_network_entirely`,
/// which drives `ensure_cached_engine` directly with a pinned engine it builds
/// itself. Nothing is lost on an unpinned host.
#[test]
fn no_download_is_honoured_when_fetch_is_enabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let host = host_target();
    let (ok, output) = build_with(tmp.path(), None, &["--features", "fetch"], &[]);
    assert!(ok, "{output}");

    if pinned_engine(&host).is_some() {
        assert!(
            output.contains("NEEDLE_NO_DOWNLOAD is set"),
            "{host} has a pinned engine, so the opt-out is what suppressed the fetch and the \
             warning must say so:\n{output}"
        );
    } else {
        assert!(
            output.contains("no verified libneedle engine is published"),
            "{host} has no pinned engine, so that is the reason the warning must give — not the \
             opt-out, which was never reached:\n{output}"
        );
    }

    // True on every host, and the reason this test exists.
    assert!(
        !output.contains("could not download"),
        "nothing should have been attempted:\n{output}"
    );
    assert!(
        !tmp.path().join("engine-cache").exists(),
        "an engine-less build must not create the engine cache"
    );
}

/// A vendored engine short-circuits resolution: no warning, no download.
///
/// `NEEDLE_CXX_RUNTIME=none` is load-bearing, and not a weakening. Resolving an
/// engine makes `build.rs` also emit `cargo:rustc-link-lib=static=c++`, and
/// `rustc` resolves `static=` libraries even for a lib-only build — so without
/// this the fixture fails on any machine with no libc++, which includes the
/// required `verify` CI job (stock Ubuntu: neither `libc++.a` nor `libc++.so`).
///
/// The alternative was installing `libc++-dev` into that gate. Rejected: the
/// gate builds with default features, where `fetch` is off, no engine resolves
/// and no C++ runtime is ever named — so nothing in the *product* needs libc++
/// there, and adding a system package to the required gate to satisfy one
/// fixture would change what the gate assumes in order to avoid fixing the
/// fixture. `libstdc++` was the other option and is wrong twice: it is the
/// runtime this whole round proved the engine does *not* use, and it does not
/// exist on macOS.
///
/// Runtime selection is a separate concern from engine resolution, and it keeps
/// its own coverage: seven tests in `engine_resolution.rs` pin the choice per
/// target and per machine state, and CI's `verify-ffi` job links it for real.
/// This test is about resolution, so it holds the other axis still.
#[test]
fn a_vendored_engine_is_used_without_any_download() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("crate");
    let target = std::env::var("TARGET").unwrap_or_else(|_| host_target());
    let vendor = root.join("vendor").join(&target);
    std::fs::create_dir_all(&vendor).expect("vendor dir");
    let name = if target.contains("msvc") {
        "needle.lib"
    } else {
        "libneedle.a"
    };
    // A real (empty) `ar` archive: rustc validates the archive identifier of
    // any native library it is pointed at, even for a build that never links.
    std::fs::write(vendor.join(name), b"!<arch>\n").expect("write engine");

    let (ok, output) = build_with(
        &root,
        None,
        &["--features", "fetch"],
        &[("NEEDLE_CXX_RUNTIME", "none")],
    );
    assert!(ok, "{output}");
    assert!(
        !output.contains("warning:"),
        "a resolved engine must not warn:\n{output}"
    );
    assert!(
        !output.contains("could not download"),
        "a vendored engine must short-circuit the download:\n{output}"
    );
    assert!(
        !output.contains("no verified libneedle engine is published"),
        "a vendored engine must be used even on a target with no pinned one:\n{output}"
    );
}

/// `rustc -vV`'s host triple — what cargo will use as `TARGET` for a build
/// with no `--target`.
fn host_target() -> String {
    let out = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
        .arg("-vV")
        .output()
        .expect("rustc -vV runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .expect("rustc reports a host triple")
        .trim()
        .to_string()
}
