#!/usr/bin/env bash
# Forge installer — builds and installs the `forge` binary.
#
# Usage:
#   ./install.sh [--prefix DIR] [--uninstall] [--help]
#
# Supported platforms: macOS (x86_64, arm64), Linux (x86_64, aarch64),
# Windows via Git Bash / MSYS2 (x86_64). Requires Cargo (rustup.rs) unless a
# prebuilt binary already exists in target/release/.
#
# Honors NO_COLOR and non-interactive terminals (no ANSI colors then).

set -euo pipefail

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
if [[ "$PLATFORM" == "windows" && "$ARCH" != "x86_64" ]]; then
    die "unsupported Windows architecture: $ARCH (supported: x86_64)"
fi

# --- arguments ----------------------------------------------------------------

PREFIX="${FORGE_PREFIX:-$HOME/.local/bin}"
UNINSTALL=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)    [[ $# -ge 2 ]] || die "--prefix requires a directory"; PREFIX="$2"; shift 2 ;;
        --prefix=*)  PREFIX="${1#*=}"; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help)
            sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

BIN_NAME="forge${EXE}"
TARGET="$PREFIX/$BIN_NAME"
ROOT="$(cd "$(dirname "$0")" && pwd)"

# --- uninstall ------------------------------------------------------------------

if [[ "$UNINSTALL" == "1" ]]; then
    if [[ -f "$TARGET" ]]; then
        rm -f "$TARGET"
        ok "removed $TARGET"
    else
        info "nothing to remove at $TARGET"
    fi
    exit 0
fi

# --- install --------------------------------------------------------------------

info "platform: $PLATFORM/$ARCH"
info "installing to: $PREFIX"

PREBUILT="$ROOT/target/release/$BIN_NAME"
if command -v cargo >/dev/null 2>&1; then
    [[ -f "$ROOT/Cargo.toml" ]] || die "Cargo.toml not found next to install.sh; run it from the forge checkout"
    info "building release binary (this can take a few minutes on first run)"
    (cd "$ROOT" && cargo build --release --locked -p forge-cli)
    [[ -f "$PREBUILT" ]] || die "build finished but $PREBUILT is missing"
elif [[ -f "$PREBUILT" ]]; then
    warn "cargo not found; using existing prebuilt binary at $PREBUILT"
else
    die "cargo not found and no prebuilt binary at $PREBUILT. Install Rust via https://rustup.rs and re-run."
fi

mkdir -p "$PREFIX"
install -m 0755 "$PREBUILT" "$TARGET"
ok "installed $TARGET"

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
