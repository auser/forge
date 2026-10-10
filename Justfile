# Forge development recipes

# cargo check across the workspace
check:
    cargo check --workspace --all-targets

# format all crates
fmt:
    cargo fmt --all

# clippy with warnings denied
#
# Default features on purpose, not --all-features: forge-needle's `needle-e2e`
# feature compiles tests that need a 35 MB weights archive at *run* time, and
# the repo deliberately does not carry one — compiling them here is lint-ffi's
# job (see below). Any *other* feature added to the workspace should either be
# default-reachable or get a line here.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Type- and lint-check the e2e test targets WITHOUT linking.
# `cargo clippy` never invokes the linker, so this needs no libneedle.a and no
# weights — yet it is the only thing in the default gate that compiles
# tests/e2e.rs and tests/fastpath_latency.rs at all (they are gated on the
# `needle-e2e` feature). ffi_backend.rs itself is compiled by plain `just
# lint` on every run now: the backend is no longer behind a feature, only the
# engine link is a build-time fact (`HAS_EMBEDDED_BACKEND`).
#
# NEEDLE_NO_DOWNLOAD=1 keeps this link-free *and* network-free: engine
# resolution always runs at build time, and there is no reason to pull an
# engine binary for a pass that never links one. An unresolvable engine is a
# warning here, not an error, precisely so this recipe keeps working on
# machines without one.
lint-ffi:
    NEEDLE_NO_DOWNLOAD=1 \
    cargo clippy -p forge-needle --features needle-e2e --all-targets -- -D warnings

# unit + integration tests (see `lint` for why not --all-features)
test:
    cargo test --workspace

# BDD scenarios: cucumber harness driving the compiled forge binary over
# tests/features/ (custom test target, harness = false)
bdd:
    cargo test -p forge-cli --test bdd

# fmt --check + check + lint (incl. link-free e2e lint) + test + bdd
verify: check lint lint-ffi test bdd
    cargo fmt --all --check

# Release gate for the drop-in coding harness. `verify` covers the complete
# workspace; the named tests make the provider wire contracts and the
# inspect/edit/check/review acceptance path visible in release logs.
harness-gate: verify
    cargo test -p forge-providers --test provider_contracts
    cargo test -p forge-cli --test cli one_command_workflow_records_plan_checks_review_and_diff

# Opt-in, real provider call for a configured model. This uses only the
# caller's existing authorized credential and never rotates accounts or
# retries around a provider limit.
harness-canary model:
    cargo run -p forge-cli -- --model {{model}} doctor --live

# The libneedle FFI backend, linked for real. The engine is fetched and
# checksum-verified automatically for the targets listed in
# crates/needle-sys/build_support.rs (cached under $CARGO_HOME/needle-engine,
# so once per machine). On any other target, supply one: NEEDLE_LIB_DIR=<dir>,
# or drop libneedle.a into crates/needle-sys/vendor/<target-triple>/.
# NEEDLE_NO_DOWNLOAD=1 to stay offline (the build then warns and continues
# engine-less).
verify-ffi:
    cargo clippy -p needle-sys -p forge-needle --all-targets --features needle-e2e -- -D warnings
    cargo test -p forge-needle

# Real-weights end-to-end suite for the FFI backend. Needs the engine (as
# above) plus FORGE_NEEDLE_E2E_WEIGHTS pointing at a .cact archive, e.g.
# ~/.cache/forge/models/needle3.cact (fetched by `forge init`). Release build:
# latency numbers from a debug build are not meaningful.
e2e:
    cargo test --release -p forge-needle --features needle-e2e --test e2e -- --nocapture

# debug build
build:
    cargo build --workspace

# release build
release:
    cargo build --workspace --release

# clean build artifacts
clean:
    cargo clean
