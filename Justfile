# Forge development recipes

# cargo check across the workspace
check:
    cargo check --workspace --all-targets

# format all crates
fmt:
    cargo fmt --all

# clippy with warnings denied
#
# Default features on purpose, not --all-features: forge-needle's `ffi` and
# `needle-e2e` features need a per-platform libneedle.a (and, for e2e, a 35 MB
# weights archive) that the repo deliberately does not carry, so
# --all-features would make this fail on a fresh checkout. `just verify-ffi`
# covers those. Any *other* feature added to the workspace should either be
# default-reachable or get a line here.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Type- and lint-check the FFI backend and its e2e test WITHOUT linking.
# `cargo clippy` never invokes the linker, so this needs no libneedle.a and no
# weights — yet it is the only thing in the default gate that compiles
# ffi_backend.rs (700+ lines, most of the crate's unsafe) and tests/e2e.rs at
# all. Without it, `ffi` code could stop compiling and `just verify` would
# still pass.
#
# NEEDLE_NO_DOWNLOAD=1 keeps this link-free *and* network-free: `ffi` turns on
# needle-sys/fetch, and there is no reason to pull an engine binary for a pass
# that never links one. An unresolvable engine is a warning here, not an
# error, precisely so this recipe keeps working on machines without one.
lint-ffi:
    NEEDLE_NO_DOWNLOAD=1 \
    cargo clippy -p forge-needle --features "ffi needle-e2e" --all-targets -- -D warnings

# unit + integration tests (see `lint` for why not --all-features)
test:
    cargo test --workspace

# BDD scenarios: cucumber harness driving the compiled forge binary over
# tests/features/ (custom test target, harness = false)
bdd:
    cargo test -p forge-cli --test bdd

# fmt --check + check + lint (incl. link-free ffi lint) + test + bdd
verify: check lint lint-ffi test bdd
    cargo fmt --all --check

# The libneedle FFI backend. The engine is fetched and checksum-verified
# automatically for the targets listed in crates/needle-sys/build_support.rs
# (cached under $CARGO_HOME/needle-engine, so once per machine). On any other
# target, supply one: NEEDLE_LIB_DIR=<dir>, or drop libneedle.a into
# crates/needle-sys/vendor/<target-triple>/. NEEDLE_NO_DOWNLOAD=1 to stay
# offline.
verify-ffi:
    cargo clippy -p needle-sys -p forge-needle --all-targets --features "ffi needle-e2e" -- -D warnings
    cargo test -p forge-needle --features ffi

# Real-weights end-to-end suite for the FFI backend. Needs the engine (as
# above) plus FORGE_NEEDLE_E2E_WEIGHTS pointing at a .cact archive, e.g.
# ~/.cache/forge/models/needle3.cact (fetched by `forge init`). Release build:
# latency numbers from a debug build are not meaningful.
e2e:
    cargo test --release -p forge-needle --features "ffi needle-e2e" --test e2e -- --nocapture

# debug build
build:
    cargo build --workspace

# release build
release:
    cargo build --workspace --release

# clean build artifacts
clean:
    cargo clean
