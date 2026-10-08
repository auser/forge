#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) triple=x86_64-unknown-linux-gnu ;;
    Linux/aarch64|Linux/arm64) triple=aarch64-unknown-linux-gnu ;;
    Darwin/x86_64) triple=x86_64-apple-darwin ;;
    Darwin/arm64|Darwin/aarch64) triple=aarch64-apple-darwin ;;
    *) echo "installer test unsupported on this host" >&2; exit 0 ;;
esac

mkdir -p "$TMP/assets/payload" "$TMP/shadow" "$TMP/home"
cat >"$TMP/assets/payload/forge" <<'EOF'
#!/usr/bin/env bash
[[ "${1:-}" == version && "${2:-}" == --build ]]
printf 'forge 0.0.0-test (commit installer-test, target test)\n'
EOF
chmod +x "$TMP/assets/payload/forge"
asset="$TMP/assets/forge-$triple.tar.gz"
tar -czf "$asset" -C "$TMP/assets/payload" forge
if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$asset" | awk '{print $1}' >"$asset.sha256"
else
    sha256sum "$asset" | awk '{print $1}' >"$asset.sha256"
fi

cat >"$TMP/shadow/forge" <<'EOF'
#!/usr/bin/env bash
echo shadow
EOF
chmod +x "$TMP/shadow/forge"

prefix="$TMP/install/bin"
output="$(
    HOME="$TMP/home" PATH="$TMP/shadow:$PATH" \
        FORGE_RELEASE_BASE="file://$TMP/assets" \
        bash "$ROOT/install.sh" --prefix "$prefix" 2>&1
)"
grep -Fq "verified installed binary: forge 0.0.0-test" <<<"$output"
grep -Fq "'forge' still resolves to $TMP/shadow/forge instead of $prefix/forge" <<<"$output"
[[ "$(grep -Fc "fix this shell with:" <<<"$output")" == 1 ]]

output="$(
    HOME="$TMP/home" PATH="$prefix:$TMP/shadow:$PATH" \
        FORGE_RELEASE_BASE="file://$TMP/assets" \
        bash "$ROOT/install.sh" --prefix "$prefix" 2>&1
)"
! grep -Fq "fix this shell with:" <<<"$output"
grep -Fq "verified installed binary: forge 0.0.0-test" <<<"$output"

echo "installer PATH tests passed"
