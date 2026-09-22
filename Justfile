# Forge development recipes

# cargo check across the workspace
check:
    cargo check --workspace --all-targets

# format all crates
fmt:
    cargo fmt --all

# clippy with warnings denied
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# unit + integration tests
test:
    cargo test --workspace --all-features

# BDD scenarios: cucumber harness driving the compiled forge binary over
# tests/features/ (custom test target, harness = false)
bdd:
    cargo test -p forge-cli --test bdd

# fmt --check + check + lint + test + bdd
verify: check lint test bdd
    cargo fmt --all --check

# debug build
build:
    cargo build --workspace

# release build
release:
    cargo build --workspace --release

# clean build artifacts
clean:
    cargo clean
