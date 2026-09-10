#!/bin/sh
# Regression test for a live defect: latest.json is served with
# cache-control: max-age=600, but the nightly tag is fixed and every release
# replaces its assets. For up to ten minutes after a push to main, the cached
# index names a nightly version whose assets no longer exist, and a naive
# `remuda-$version-$target.tar.gz` 404s outright. This shipped twice on
# 2026-09-10, four minutes apart, overlapping into one continuous window.
#
# This test injects a deliberately WRONG nightly version in place of the real
# index (a curl shim rewrites only the latest.json fetch; every other URL,
# including the real SHA256SUMS and asset download, goes to the real curl)
# and requires the install to succeed anyway by discovering the real asset
# name from SHA256SUMS instead of trusting the stale version string.
#
# Run it yourself:  scripts/check-nightly-stale-index.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

real_curl=$(command -v curl) || { echo "check-nightly-stale-index: need curl" >&2; exit 1; }

mkdir -p "$tmp/shim"
cat >"$tmp/shim/curl" <<'EOF'
#!/bin/sh
for a in "$@"; do
	case "$a" in
	*/latest.json)
		echo '{"stable":"0.0.0","nightly":"0.0.0-stale-test-version","updated":"1970-01-01T00:00:00Z"}'
		exit 0
		;;
	esac
done
exec __REAL_CURL__ "$@"
EOF
sed "s#__REAL_CURL__#$real_curl#" "$tmp/shim/curl" >"$tmp/shim/curl.tmp" && mv "$tmp/shim/curl.tmp" "$tmp/shim/curl"
chmod +x "$tmp/shim/curl"

export HOME="$tmp/home"
export XDG_DATA_HOME="$tmp/data"
export REMUDA_INSTALL_DIR="$tmp/bin"
export REMUDA_CHANNEL=nightly
mkdir -p "$HOME" "$XDG_DATA_HOME" "$REMUDA_INSTALL_DIR"

PATH="$tmp/shim:$PATH" sh "$ROOT/docs/install.sh"

[ -x "$REMUDA_INSTALL_DIR/remuda" ] || {
	echo "check-nightly-stale-index: install did not produce $REMUDA_INSTALL_DIR/remuda despite a stale index" >&2
	exit 1
}
echo "ok — nightly install succeeded despite a deliberately stale latest.json"
