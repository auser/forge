//! Resolve `libneedle` and emit its link flags. That is all this does — the
//! `extern "C"` declarations are hand-written in `src/lib.rs` (see that file
//! for why there is no `bindgen` here), so this build script parses no
//! headers and generates no code.
//!
//! **Resolution order** (the design spec's §3, now complete) — always run, on
//! every build: the engine is on by default, so nothing asks for it.
//!
//! 1. `NEEDLE_LIB_DIR` — an engine the operator fetched themselves.
//! 2. `crates/needle-sys/vendor/<target>/` — a vendored engine.
//! 3. **Download at build time, SHA-256 verified** — the pinned engine for
//!    the targets forge has verified (`PINNED_ENGINES`).
//!
//! Step 3 and the pinned per-target checksums live in `build_support.rs`,
//! included below so the same code can be unit-tested (`cargo test` never
//! compiles a build script). Guard rails there: `NEEDLE_NO_DOWNLOAD=1` opts
//! out for offline/air-gapped builds, the cache is content-addressed and
//! shared across builds so nothing redownloads, and a mismatched checksum is
//! discarded rather than linked.
//!
//! **Artifact provenance** (verified 2026-09-23, re-verified 2026-09-24): the
//! Needle 3 engine is published per platform in the same Hugging Face repo as
//! the weights, `Cactus-Compute/needle3` (Apache-2.0 — `cardData.license`
//! plus a full Apache 2.0 `LICENSE` file at the repo root). Each platform
//! folder holds `libneedle.a` + `needle.h`:
//!
//! ```text
//! curl -L -o crates/needle-sys/vendor/aarch64-apple-darwin/libneedle.a \
//!   https://huggingface.co/Cactus-Compute/needle3/resolve/main/macos-arm64/libneedle.a
//! ```
//!
//! Published folders: `macos-arm64`, `linux-{x86_64,arm64,armv7,riscv64,mipsel}`,
//! `windows-{x86_64,arm64}`, `android-{arm64,armv7,riscv64}`, `ios-arm64`,
//! `ios-sim-arm64`, `tvos-arm64`, `watchos-arm64`, `wasm`, `wasm-component`.
//! There is **no** `macos-x86_64` folder — Intel macOS has no engine to fetch.
//! Which of these forge will download automatically, and which were verified
//! but deliberately left out, is documented on `PINNED_ENGINES`.
//!
//! The binary is **not** committed (see `.gitignore`): it is a ~1 MB
//! per-platform blob that resolution fetches on demand. `needle.h` *is*
//! committed, as the contract of record for the hand-written declarations.
//!
//! **The fact downstream code reads.** When (and only when) an engine
//! resolved and link flags went out, this script also emits
//! `cargo:rustc-cfg=needle_engine`; `src/lib.rs` mirrors that into
//! [`needle_sys::ENGINE_LINKED`], the *only* correct answer to "can this
//! build run inference" — a build-time fact about what resolved, not a
//! feature someone requested. The check-cfg directive is emitted
//! unconditionally so the cfg never trips `unexpected_cfgs`.
//!
//! **Diagnostics policy.** The engine is always wanted now, so there is no
//! quiet path:
//!
//! * Engine unresolvable → one `cargo:warning` naming the single thing to do
//!   next, and the build continues engine-less: `src/lib.rs` links stub
//!   symbols instead, `ENGINE_LINKED` is `false`, and forge routes with
//!   static rules and stays usable.
//! * Engine unresolvable, `NEEDLE_REQUIRE_ENGINE=1` → hard failure. Release
//!   and CI builds that intend to ship a brain set this, so a brain-less
//!   binary can never go out labelled brain-enabled.
//! * `NEEDLE_LIB_DIR` set but wrong → `cargo:warning` *and* a failure.
//!   Operator error, deserves to be impossible to miss.

// `build_support.rs` is pulled in rather than `mod`-declared so that
// `tests/engine_resolution.rs` can include the very same source and unit-test
// it; a build script is a crate of its own that nothing else can depend on.
// Not every item is used by both consumers, hence the blanket allow.
#![allow(dead_code)]

include!("build_support.rs");

fn main() {
    for key in WATCHED_ENVS {
        println!("cargo:rerun-if-env-changed={key}");
    }

    // A typo here would otherwise silently fall through to the default and
    // link a runtime the operator did not ask for.
    if let Ok(value) = std::env::var(CXX_RUNTIME_ENV)
        && !known_cxx_runtime(&value)
    {
        println!(
            "cargo:warning=needle-sys: {CXX_RUNTIME_ENV}={value:?} is not a value I know \
             (static-libc++ | libc++ | libstdc++); using this target's default instead."
        );
    }

    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env("OUT_DIR"));
    let target = env("TARGET");
    let host = env("HOST");
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

    // Step 1 and 2. Step 1 is the only path that fails on absence: the
    // operator named a specific directory, so an empty one is a mistake
    // worth stopping for.
    let mut lib_dir: Option<PathBuf> = match configured {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if static_lib(&dir, &target).is_none() {
                let message = format!(
                    "{LIB_DIR_ENV} is set to {} but no {} is there.\n\
                     Fetch one for {target} from the Cactus Needle 3 release:\n  \
                     https://huggingface.co/Cactus-Compute/needle3/tree/main/<platform>\n\
                     (Apache-2.0; platform folders are listed in crates/needle-sys/build.rs.)",
                    dir.display(),
                    lib_file_name(&target),
                );
                // Both a cargo warning and a failure: a build script's stderr
                // is easy to lose in a parallel build's output, and this one
                // is always actionable — the operator asked for a specific
                // directory.
                println!("cargo:warning=needle-sys: {}", one_line(&message));
                fail(&message);
            }
            Some(dir)
        }
        None => static_lib(&vendor, &target).map(|_| vendor.clone()),
    };

    // Step 3: the engine is always wanted, so the fetch attempt always runs
    // when steps 1 and 2 found nothing.
    if lib_dir.is_none() {
        match fetch_engine_dir(&target, &out_dir) {
            Ok(dir) => {
                println!(
                    "cargo:rerun-if-changed={}",
                    dir.join(lib_file_name(&target)).display()
                );
                lib_dir = Some(dir);
            }
            Err(e) => {
                // Warn either way: whether this is fatal or not, an engine-less
                // build is a real (if supported) outcome, and a build script's
                // own stderr is easy to lose in a parallel build's output.
                let message = e.message(&target);
                println!("cargo:warning=needle-sys: {}", one_line(&message));
                if env_flag(REQUIRE_ENV) {
                    fail(&format!(
                        "{message}\n\n({REQUIRE_ENV} is set, so this is fatal.)"
                    ));
                }
            }
        }
    }

    // The build-time fact: `src/lib.rs` compiles its real declarations or its
    // stubs on this cfg, and mirrors it as `ENGINE_LINKED` for the rest of
    // the workspace. Declared unconditionally so the cfg is always known.
    println!("cargo::rustc-check-cfg=cfg(needle_engine)");

    // The fetch attempt above already warned when nothing resolved; nothing
    // more to say on that path.
    if let Some(dir) = &lib_dir {
        emit_link_flags(dir, &target, &host);
    }
}

/// Stop the build with a message and no backtrace noise.
///
/// A `panic!` in a build script prints `panic at build.rs:NN` plus cargo's
/// own "failed to run custom build command" framing around the text, which
/// buries a multi-line remedy. Printing the message ourselves and exiting
/// non-zero puts it on stderr exactly as written; cargo still reports the
/// build script as failed.
fn fail(message: &str) -> ! {
    eprintln!("\nerror: needle-sys: {message}\n");
    std::process::exit(1);
}

fn env(key: &str) -> String {
    match std::env::var(key) {
        Ok(value) => value,
        Err(e) => fail(&format!("cargo did not set {key}: {e}")),
    }
}

/// Emit the link flags for a resolved engine: the engine itself, then the C++
/// standard library it needs.
///
/// The C++ half is only probed here, once an engine actually exists — there is
/// no point telling someone to install libc++ for a build that was never going
/// to link anything.
fn emit_link_flags(dir: &Path, target: &str, host: &str) {
    // The fact that makes this build's `ENGINE_LINKED` true — emitted only
    // alongside real link flags, never for an engine-less build.
    println!("cargo:rustc-cfg=needle_engine");
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=needle");

    let requested = requested_cxx_runtime();
    let plan = cxx_plan(target, requested.as_deref(), &probe_cxx(target, host));
    for dir in &plan.search_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    for flag in &plan.flags {
        println!("cargo:rustc-link-lib={flag}");
    }
    if let Some(warning) = &plan.warning {
        println!("cargo:warning=needle-sys: {}", one_line(warning));
    }
}
