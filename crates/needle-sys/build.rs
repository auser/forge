//! Resolve `libneedle` + `needle.h` and generate bindings for them.
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
//! `needle.h` *is* committed, because it is the API contract this crate is
//! written against and bindgen needs it for a plain `cargo check` on any
//! machine — including ones that will never link the engine.

use std::path::{Path, PathBuf};

/// Where an operator can point this build at an engine they fetched
/// themselves. Takes precedence over the vendored directory.
const LIB_DIR_ENV: &str = "NEEDLE_LIB_DIR";

fn main() {
    println!("cargo:rerun-if-env-changed={LIB_DIR_ENV}");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=needle.h");

    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let target = env("TARGET");
    let vendor = manifest.join("vendor").join(&target);

    // An explicitly configured directory that doesn't hold the engine is an
    // operator mistake worth failing loudly on; a merely absent vendor
    // directory is the normal state of a fresh checkout and only matters at
    // link time (see `emit_link_flags`).
    let configured = std::env::var(LIB_DIR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let lib_dir: Option<PathBuf> = match configured {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if static_lib(&dir, &target).is_none() {
                panic!(
                    "{LIB_DIR_ENV} is set to {} but no {} is there.\n\
                     Fetch one for {target} from the Cactus Needle 3 release:\n  \
                     https://huggingface.co/Cactus-Compute/needle3/tree/main/<platform>\n\
                     (Apache-2.0; platform folders are listed in crates/needle-sys/build.rs.)",
                    dir.display(),
                    lib_file_name(&target),
                );
            }
            Some(dir)
        }
        None => static_lib(&vendor, &target).map(|_| vendor.clone()),
    };

    // Header resolution: prefer whatever ships next to the library actually
    // being linked, so the bindings always match that engine's ABI. Fall
    // back to this crate's committed copy.
    let header_dir = lib_dir
        .as_ref()
        .filter(|dir| dir.join("needle.h").is_file())
        .cloned()
        .unwrap_or_else(|| manifest.clone());
    if !header_dir.join("needle.h").is_file() {
        panic!(
            "needle.h not found in {} (and no committed fallback at {}/needle.h)",
            header_dir.display(),
            manifest.display()
        );
    }
    println!(
        "cargo:rerun-if-changed={}",
        header_dir.join("needle.h").display()
    );

    match &lib_dir {
        Some(dir) => emit_link_flags(dir, &target),
        None => println!(
            "cargo:warning=needle-sys: no {} found. Set {LIB_DIR_ENV}, or place the engine in \
             crates/needle-sys/vendor/{target}/. `cargo check`/`clippy` succeed without it, but \
             linking anything that calls the engine (forge-needle feature `ffi`) will fail with \
             undefined _needle_* symbols. Fetch it from \
             https://huggingface.co/Cactus-Compute/needle3 (Apache-2.0).",
            lib_file_name(&target),
        ),
    }

    generate_bindings(&manifest, &header_dir);
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
    println!(
        "cargo:rerun-if-changed={}",
        dir.join(lib_file_name(target)).display()
    );

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

fn generate_bindings(manifest: &Path, header_dir: &Path) {
    let out = PathBuf::from(env("OUT_DIR")).join("bindings.rs");
    let bindings = bindgen::Builder::default()
        .header(manifest.join("wrapper.h").display().to_string())
        .clang_arg(format!("-I{}", header_dir.display()))
        // The header declares exactly six functions and no types; keeping the
        // allowlist tight means a future header that grows unrelated
        // declarations can't silently widen this crate's surface.
        .allowlist_function("needle_.*")
        .generate_comments(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate();

    match bindings {
        Ok(bindings) => {
            if let Err(e) = bindings.write_to_file(&out) {
                panic!("writing {}: {e}", out.display());
            }
        }
        Err(e) => panic!(
            "bindgen failed on {}/needle.h: {e}\n\
             (bindgen needs libclang; on macOS install the Xcode command line tools, \
             on Debian/Ubuntu `apt install libclang-dev`.)",
            header_dir.display()
        ),
    }
}
