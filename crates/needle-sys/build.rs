//! Resolve `libneedle` and emit its link flags. That is all this does — the
//! `extern "C"` declarations are hand-written in `src/lib.rs` (see that file
//! for why there is no `bindgen` here), so this build script parses no
//! headers and generates no code.
//!
//! **Resolution order** (the design spec's §3, now complete):
//!
//! 1. `NEEDLE_LIB_DIR` — an engine the operator fetched themselves.
//! 2. `crates/needle-sys/vendor/<target>/` — a vendored engine.
//! 3. **Download at build time, SHA-256 verified** — only when the `fetch`
//!    feature is on, which only `forge-needle`'s `ffi` feature turns on.
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
//! per-platform blob and forge only needs it when the `ffi` feature is on.
//! `needle.h` *is* committed, as the contract of record for the hand-written
//! declarations.
//!
//! **Diagnostics policy.** `needle-sys` is a workspace member, so it builds on
//! every `cargo build`/`check` even when nothing links it and the `ffi`
//! feature is off, which is the overwhelmingly common case. So the volume of
//! what this prints is keyed on whether the engine was actually asked for:
//!
//! * `fetch` off (default build), no engine → quiet `println!` note, visible
//!   only under `cargo build -vv`. Nothing is wrong: `cargo check`/`clippy`
//!   never link.
//! * `fetch` on, engine unresolvable → `cargo:warning` naming the one thing
//!   to do next, and the build continues engine-less. It continues rather
//!   than failing because `just lint-ffi`/CI's link-free clippy pass compiles
//!   the `ffi` code *without* linking on machines that have no engine, and
//!   that guarantee is worth more than pre-empting a link error whose cause
//!   the warning already states.
//! * `fetch` on, engine unresolvable, `NEEDLE_REQUIRE_ENGINE=1` → hard
//!   failure. Release and CI builds that intend to ship a brain set this, so
//!   a brain-less binary can never go out labelled brain-enabled.
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

    // Step 3: only when something downstream actually wants to link.
    let wants_engine = std::env::var(FETCH_FEATURE_ENV).is_ok();
    if lib_dir.is_none() && wants_engine {
        match fetch_engine_dir(&target, &out_dir) {
            Ok(dir) => {
                println!(
                    "cargo:rerun-if-changed={}",
                    dir.join(lib_file_name(&target)).display()
                );
                lib_dir = Some(dir);
            }
            Err(e) => {
                // Warn either way: whether this is fatal or not, the user did
                // ask for an engine, and a build script's own stderr is easy
                // to lose in a parallel build's output.
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

    match &lib_dir {
        Some(dir) => emit_link_flags(dir, &target, &host),
        // Deliberately NOT `cargo:warning=` — see the module docs. This is
        // a note for someone reading `cargo build -vv`, not a diagnostic
        // for every user of a default build. The `fetch`-on case already
        // warned above.
        None if !wants_engine => println!(
            "needle-sys: no {} for {target}; building without the engine. This is normal unless \
             you enabled forge-needle's `ffi` feature — then set {LIB_DIR_ENV} or place the \
             engine in crates/needle-sys/vendor/{target}/ (see README, \"Embedded Needle brain\"). \
             https://huggingface.co/Cactus-Compute/needle3 (Apache-2.0).",
            lib_file_name(&target),
        ),
        None => {}
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
