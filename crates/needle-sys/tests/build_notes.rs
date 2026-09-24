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

use std::path::{Path, PathBuf};
use std::process::Command;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Copy the crate's real sources into `dir` with a self-contained manifest
/// and no vendored engine. Returns the manifest path.
fn standalone_copy(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("src dir");
    for (from, to) in [
        (crate_dir().join("build.rs"), dir.join("build.rs")),
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
         \n[dependencies]\n",
    )
    .expect("write manifest");
    manifest
}

/// Build the standalone copy, with `NEEDLE_LIB_DIR` removed unless given.
/// Returns (success, stdout+stderr).
fn build(root: &Path, lib_dir: Option<&Path>) -> (bool, String) {
    let manifest = standalone_copy(root);
    let mut cmd = Command::new(env!("CARGO"));
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(root.join("target"))
        .env_remove("NEEDLE_LIB_DIR")
        // Colour codes would make the string assertions brittle.
        .env("CARGO_TERM_COLOR", "never");
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
        .arg(tmp.path().join("target"))
        .env_remove("NEEDLE_LIB_DIR")
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
