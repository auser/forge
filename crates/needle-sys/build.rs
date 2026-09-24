//! Resolve `libneedle` and emit its link flags. That is all this does — the
//! `extern "C"` declarations are hand-written in `src/lib.rs` (see that file
//! for why there is no `bindgen` here), so this build script has no
//! dependencies and needs no libclang, no header parsing and no codegen.
//!
//! **Artifact provenance** (verified 2026-09-23): the Needle 3 engine is
//! published per platform in the same Hugging Face repo as the weights,
//! `Cactus-Compute/needle3` (Apache-2.0 — `cardData.license` plus a full
//! Apache 2.0 `LICENSE` file at the repo root). Each platform folder holds
//! `libneedle.a` + `needle.h`:
//!
//! ```text
//! curl -L -o crates/needle-sys/vendor/aarch64-apple-darwin/libneedle.a \
//!   https://huggingface.co/Cactus-Compute/needle3/resolve/main/macos-arm64/libneedle.a
//! ```
//!
//! Published folders: `macos-arm64`, `linux-x86_64`, `linux-arm64`,
//! `linux-armv7`, `linux-riscv64`, `linux-mipsel`, `windows-x86_64`,
//! `windows-arm64`, `android-{arm64,armv7,riscv64}`, `ios-arm64`,
//! `ios-sim-arm64`, `tvos-arm64`, `watchos-arm64`, `wasm`.
//!
//! The binary is **not** committed (see `.gitignore`): it is a 1.1 MB
//! per-platform blob and forge only needs it when the `ffi` feature is on.
//! `needle.h` *is* committed, as the contract of record for the hand-written
//! declarations.
//!
//! A *missing* library is not an error here, and — since 2026-09-24 — not a
//! `cargo:warning` either. `needle-sys` is a workspace member, so it builds
//! on every `cargo build`/`check` even when nothing links it and the `ffi`
//! feature is off, which is the overwhelmingly common case; a build script
//! cannot see downstream features, so a warning here fired at users who had
//! no reason to care and read as a build *failure*. The absence is therefore
//! a plain `println!` note (captured by cargo, shown only under `-vv`), and
//! the one case that really is broken stays loud:
//!
//! * absent library, no `NEEDLE_LIB_DIR` → quiet note. `cargo check`/
//!   `clippy` never link, so nothing is wrong yet. A build that *does* link
//!   engine calls (`forge-needle` feature `ffi`) fails at link time with
//!   undefined `_needle_*` symbols — see README's "Embedded Needle brain
//!   (`ffi`)" section, which is the documented pointer for that failure.
//! * `NEEDLE_LIB_DIR` set but wrong → `cargo:warning` *and* a panic.
//!   Operator error, deserves to be impossible to miss.

use std::path::{Path, PathBuf};

/// Where an operator can point this build at an engine they fetched
/// themselves. Takes precedence over the vendored directory.
const LIB_DIR_ENV: &str = "NEEDLE_LIB_DIR";

fn main() {
    println!("cargo:rerun-if-env-changed={LIB_DIR_ENV}");

    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let target = env("TARGET");
    let vendor = manifest.join("vendor").join(&target);

    let configured = std::env::var(LIB_DIR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());

    // Watch both candidate library paths unconditionally, even when
    // neither exists yet: cargo happily tracks non-existent paths, so
    // fetching the library later (e.g. the README's `curl` step) makes
    // this build script rerun and pick it up automatically, rather than
    // silently keeping a stale "no library" link decision until something
    // else invalidates the build cache.
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join(lib_file_name(&target)).display()
    );
    if let Some(dir) = &configured {
        println!(
            "cargo:rerun-if-changed={}",
            PathBuf::from(dir).join(lib_file_name(&target)).display()
        );
    }

    let lib_dir: Option<PathBuf> = match configured {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if static_lib(&dir, &target).is_none() {
                // The only loud path left. `cargo:warning` as well as the
                // panic: a panic message inside a build script is easy to
                // lose in a parallel build's output, and this one is always
                // actionable — the operator asked for a specific directory.
                let message = format!(
                    "{LIB_DIR_ENV} is set to {} but no {} is there.\n\
                     Fetch one for {target} from the Cactus Needle 3 release:\n  \
                     https://huggingface.co/Cactus-Compute/needle3/tree/main/<platform>\n\
                     (Apache-2.0; platform folders are listed in crates/needle-sys/build.rs.)",
                    dir.display(),
                    lib_file_name(&target),
                );
                println!("cargo:warning=needle-sys: {}", one_line(&message));
                panic!("{message}");
            }
            Some(dir)
        }
        None => static_lib(&vendor, &target).map(|_| vendor.clone()),
    };

    match &lib_dir {
        Some(dir) => emit_link_flags(dir, &target),
        // Deliberately NOT `cargo:warning=` — see the module docs. This is
        // a note for someone reading `cargo build -vv`, not a diagnostic
        // for every user of a default build.
        None => println!(
            "needle-sys: no {} for {target}; building without the engine. This is normal unless \
             you enabled forge-needle's `ffi` feature — then set {LIB_DIR_ENV} or place the \
             engine in crates/needle-sys/vendor/{target}/ (see README, \"Embedded Needle brain\"). \
             https://huggingface.co/Cactus-Compute/needle3 (Apache-2.0).",
            lib_file_name(&target),
        ),
    }
}

/// Collapse newlines: `cargo:warning=` is a single-line directive, so an
/// embedded newline would truncate the message at the first break.
fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn env(key: &str) -> String {
    match std::env::var(key) {
        Ok(value) => value,
        Err(e) => panic!("cargo did not set {key}: {e}"),
    }
}

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

fn emit_link_flags(dir: &Path, target: &str) {
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=needle");
    // `cargo:rerun-if-changed` for this path is already emitted
    // unconditionally in `main` (covers both the found and not-found case).

    // libneedle is a C++ translation unit behind an `extern "C"` facade:
    // `nm libneedle.a` shows libc++ symbols plus `__cxa_*` /
    // `__gxx_personality_v0`, so the C++ runtime has to be linked too.
    // MSVC links its runtime automatically from object-file directives.
    if target.contains("apple") || target.contains("freebsd") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else if target.contains("msvc") {
        // nothing to add
    } else if target.contains("android") {
        println!("cargo:rustc-link-lib=static=c++_static");
        println!("cargo:rustc-link-lib=static=c++abi");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}
