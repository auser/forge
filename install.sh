#!/usr/bin/env bash
# Forge installer — installs the `forge` binary from GitHub releases,
# falling back to `cargo install`. Works from a checkout (./install.sh)
# or piped from curl. See usage() below or run with --help.

set -euo pipefail

GITHUB_REPO="auser/forge"
FORGE_REPO_HTTPS="https://github.com/${GITHUB_REPO}.git"
DEFAULT_RELEASE_BASE="https://github.com/${GITHUB_REPO}/releases/latest/download"

usage() {
    cat <<'EOF'
Forge installer — installs the `forge` binary.

Usage:
  curl -fsSL https://raw.githubusercontent.com/auser/forge/main/install.sh | bash
  curl -fsSL ... | bash -s -- --prefix /usr/local/bin
  ./install.sh [--prefix DIR] [--uninstall] [--help]   # from a checkout

Install strategy, in order:
  1. prebuilt release asset for your platform (checksum-verified)
  2. cargo install from git (or from this checkout, when run locally)

Supported platforms: macOS (x86_64, arm64), Linux (x86_64, aarch64),
Windows via Git Bash / MSYS2 (x86_64).

Options:
  --prefix DIR   install directory (default: ~/.local/bin, or $FORGE_PREFIX)
  --uninstall    remove the installed binary
  -h, --help     show this help

Environment:
  FORGE_PREFIX         same as --prefix
  FORGE_REPO           git URL for the cargo fallback
  FORGE_RELEASE_BASE  override the release download base URL
  NO_COLOR             disable colored output
EOF
}

# --- colors ---------------------------------------------------------------

if [[ -n "${NO_COLOR:-}" || ! -t 1 ]]; then
    C_RESET='' C_INFO='' C_OK='' C_WARN='' C_ERR=''
else
    C_RESET=$'\033[0m'
    C_INFO=$'\033[1;34m'
    C_OK=$'\033[1;32m'
    C_WARN=$'\033[1;33m'
    C_ERR=$'\033[1;31m'
fi

info()  { printf '%s==>%s %s\n' "$C_INFO" "$C_RESET" "$*"; }
ok()    { printf '%s ok %s %s\n' "$C_OK" "$C_RESET" "$*"; }
warn()  { printf '%swarn%s %s\n' "$C_WARN" "$C_RESET" "$*" >&2; }
die()   { printf '%serror%s %s\n' "$C_ERR" "$C_RESET" "$*" >&2; exit 1; }

# --- platform detection -----------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"
EXE=""
case "$OS" in
    Darwin)              PLATFORM="macos" ;;
    Linux)               PLATFORM="linux" ;;
    MINGW*|MSYS*|CYGWIN*) PLATFORM="windows"; EXE=".exe" ;;
    *) die "unsupported operating system: $OS (supported: macOS, Linux, Windows via Git Bash/MSYS2)" ;;
esac
case "$ARCH" in
    x86_64|amd64)   ARCH="x86_64" ;;
    arm64|aarch64)  ARCH="aarch64" ;;
    *) die "unsupported architecture: $ARCH (supported: x86_64, aarch64)" ;;
esac

case "$PLATFORM/$ARCH" in
    macos/x86_64)    TRIPLE="x86_64-apple-darwin" ;;
    macos/aarch64)   TRIPLE="aarch64-apple-darwin" ;;
    linux/x86_64)    TRIPLE="x86_64-unknown-linux-gnu" ;;
    linux/aarch64)   TRIPLE="aarch64-unknown-linux-gnu" ;;
    windows/x86_64)  TRIPLE="x86_64-pc-windows-msvc" ;;
    *) die "unsupported platform: $PLATFORM/$ARCH" ;;
esac

# --- arguments ----------------------------------------------------------------

PREFIX="${FORGE_PREFIX:-$HOME/.local/bin}"
UNINSTALL=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)    [[ $# -ge 2 ]] || die "--prefix requires a directory"; PREFIX="$2"; shift 2 ;;
        --prefix=*)  PREFIX="${1#*=}"; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help)   usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

BIN_NAME="forge${EXE}"
TARGET="$PREFIX/$BIN_NAME"
RELEASE_BASE="${FORGE_RELEASE_BASE:-$DEFAULT_RELEASE_BASE}"
ASSET="forge-${TRIPLE}.tar.gz"

# --- uninstall (no download needed) --------------------------------------------

if [[ "$UNINSTALL" == "1" ]]; then
    if [[ -f "$TARGET" ]]; then
        rm -f "$TARGET"
        ok "removed $TARGET"
    else
        info "nothing to remove at $TARGET"
    fi
    exit 0
fi

info "platform: $PLATFORM/$ARCH ($TRIPLE)"
info "installing to: $PREFIX"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        die "neither shasum nor sha256sum found; cannot verify downloads"
    fi
}

# --- strategy 1: prebuilt release asset ------------------------------------------

INSTALLED=""
if command -v curl >/dev/null 2>&1; then
    info "trying release asset: $RELEASE_BASE/$ASSET"
    if curl -fsSL "$RELEASE_BASE/$ASSET" -o "$WORK/$ASSET" 2>/dev/null; then
        if curl -fsSL "$RELEASE_BASE/$ASSET.sha256" -o "$WORK/$ASSET.sha256" 2>/dev/null; then
            EXPECTED="$(awk '{print $1}' "$WORK/$ASSET.sha256")"
            ACTUAL="$(sha256_of "$WORK/$ASSET")"
            [[ "$EXPECTED" == "$ACTUAL" ]] \
                || die "checksum mismatch for $ASSET (expected $EXPECTED, got $ACTUAL); aborting"
            ok "checksum verified"
        else
            warn "no checksum file published for $ASSET; skipping verification"
        fi
        tar -xzf "$WORK/$ASSET" -C "$WORK"
        [[ -f "$WORK/$BIN_NAME" ]] || die "release asset did not contain $BIN_NAME"
        mkdir -p "$PREFIX"
        install -m 0755 "$WORK/$BIN_NAME" "$TARGET"
        INSTALLED=1
        ok "installed from release asset"
    else
        warn "no release asset for $TRIPLE (yet); falling back to cargo"
    fi
else
    warn "curl not found; falling back to cargo"
fi

# --- strategy 2: cargo install ----------------------------------------------------

if [[ -z "$INSTALLED" ]]; then
    command -v cargo >/dev/null 2>&1 \
        || die "cargo not found and no release asset available. Install Rust via https://rustup.rs and re-run."

    # Release assets for these targets ship with the embedded needle brain, so
    # a source install has to as well — otherwise falling back to cargo would
    # silently hand someone a statically-routing forge and they would have no
    # way to know why the headline feature is inert. The engine is fetched and
    # checksum-verified by needle-sys's build script; the list is the one in
    # crates/needle-sys/build_support.rs, and anything not on it has no
    # published engine (Intel macOS) or an unverified one (Windows).
    NEEDLE_FEATURES=()
    case "$TRIPLE" in
        aarch64-apple-darwin|x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu)
            NEEDLE_FEATURES=(--features needle-ffi) ;;
    esac

    SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || true
    if [[ -n "${SCRIPT_DIR:-}" && -f "$SCRIPT_DIR/Cargo.toml" ]]; then
        CARGO_ARGS=(install --path "$SCRIPT_DIR/crates/forge-cli")
        SOURCE_DESC="this checkout"
    else
        REPO="${FORGE_REPO:-$FORGE_REPO_HTTPS}"
        CARGO_ARGS=(install --git "$REPO" forge-cli)
        SOURCE_DESC="$REPO"
    fi
    CARGO_ARGS+=(--locked --root "$WORK/cargo-root")

    if [[ ${#NEEDLE_FEATURES[@]} -gt 0 ]]; then
        info "installing with cargo from $SOURCE_DESC (with the embedded brain)"
        # Retry without the feature if the engine could not be fetched or
        # linked: a brain-less forge is fully functional on static routing, so
        # an unreachable Hugging Face must not turn a working install into no
        # install at all. `forge doctor` reports which one you ended up with.
        if ! cargo "${CARGO_ARGS[@]}" "${NEEDLE_FEATURES[@]}"; then
            warn "could not build with the embedded brain (engine download or link failed);"
            warn "retrying without it — forge will route with static rules."
            warn "run \`forge doctor\` afterwards; the 'needle brain' line says how to add it."
            cargo "${CARGO_ARGS[@]}"
        fi
    else
        info "installing with cargo from $SOURCE_DESC"
        cargo "${CARGO_ARGS[@]}"
    fi

    mkdir -p "$PREFIX"
    install -m 0755 "$WORK/cargo-root/bin/$BIN_NAME" "$TARGET"
    ok "installed via cargo"
fi

# --- verify -----------------------------------------------------------------------

if [[ -x "$TARGET" ]]; then
    VERSION="$("$TARGET" version 2>/dev/null || true)"
    [[ -n "$VERSION" ]] && ok "verified: $VERSION"
fi

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        warn "$PREFIX is not on your PATH"
        # shellcheck disable=SC2016 # the literal $PATH is the hint to print
        printf '  add it with:  export PATH="%s:$PATH"\n' "$PREFIX"
        ;;
esac

ok "done — run 'forge init' inside a project to get started"
