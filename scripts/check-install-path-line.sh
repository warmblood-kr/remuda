#!/bin/sh
# The installer prints a `Next: export PATH=...` line for a person to paste.
# The install dir in it comes from REMUDA_INSTALL_DIR, so pasting the line must
# add exactly that dir to PATH and run nothing else, whatever the dir is named.
# No network: a curl shim serves a fake release.
#
# Run it yourself:  scripts/check-install-path-line.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$@"; }
else
	sha256() { shasum -a 256 "$@"; }
fi

version=0.1.0-nightly.20260101000000.abcdef1
mkdir -p "$tmp/shim" "$tmp/pkg" "$tmp/home" "$tmp/data" "$tmp/cwd"
printf '#!/bin/sh\n' >"$tmp/pkg/remuda"
tar -czf "$tmp/asset" -C "$tmp/pkg" remuda
sum=$(sha256 "$tmp/asset" | awk '{ print $1 }')
for target in x86_64-unknown-linux-gnu aarch64-linux-android aarch64-apple-darwin; do
	echo "$sum  remuda-$version-$target.tar.gz"
done >"$tmp/SHA256SUMS"
cat >"$tmp/shim/curl" <<'EOF'
#!/bin/sh
for a in "$@"; do url=$a; done
case "$url" in
*/latest.json) printf '{"stable":"0.0.0","nightly":"%s"}\n' "$FAKE_VERSION" ;;
*/SHA256SUMS) cat "$FAKE_DIR/SHA256SUMS" ;;
*) cat "$FAKE_DIR/asset" ;;
esac
EOF
chmod +x "$tmp/shim/curl"
export FAKE_VERSION="$version" FAKE_DIR="$tmp"
export HOME="$tmp/home" XDG_DATA_HOME="$tmp/data" REMUDA_CHANNEL=nightly
unset REMUDA_INSTALL_BUTLER

failed=0
fail() {
	echo "check-install-path-line: $*" >&2
	failed=1
}

install_into() {
	REMUDA_INSTALL_DIR="$1" PATH="$tmp/shim:$PATH" sh "$ROOT/docs/install.sh" 2>&1
}

# Paste the printed line into a fresh shell: PATH must gain exactly the dir.
pasted() {
	name=$1 dir=$2
	out=$(install_into "$dir") || { fail "$name: the install failed: $out"; return; }
	line=$(printf '%s\n' "$out" | sed -n 's/^Next: //p')
	got=$(cd "$tmp/cwd" && PATH=/usr/bin:/bin && eval "$line" 2>/dev/null && printf '%s' "$PATH") || true
	[ "$got" = "$dir:/usr/bin:/bin" ] || fail "$name: pasting [$line] gave PATH [$got]"
	[ ! -e "$tmp/cwd/PWNED" ] || fail "$name: pasting [$line] ran a command"
	rm -f "$tmp/cwd/PWNED"
}

pasted "a plain dir with a space" "$tmp/plain bin"
pasted "a command substitution" "$tmp/d\$(touch PWNED)"
pasted "a backtick" "$tmp/b\`touch PWNED\`"
pasted "a double quote" "$tmp/q\"x"
pasted "a single quote" "$tmp/s'x"

# A dir with a newline cannot be one pasteable line: print no command at all.
newline_dir="$tmp/n
x"
out=$(install_into "$newline_dir") || fail "a newline: the install failed: $out"
case "$out" in
*"Next: export"*) fail "a newline: a command was printed: $out" ;;
esac

[ "$failed" -eq 0 ] || exit 1
echo "ok — pasting the installer's PATH line adds the install dir and runs nothing else"
