#!/bin/sh
# Integration check for cached nightly pointers. CI checks out the published
# latest.json value; while a run is in progress, main can advance and make that
# snapshot stale. Immutable nightly releases mean the old pointer still works.
# The curl shim serves this checkout's nightly version as the cached snapshot
# and confirms the installer tries the versioned tag before its migration
# fallback to the rolling alias (needed until the first new release publishes).
#
# Run it yourself:  scripts/check-nightly-stale-index.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

real_curl=$(command -v curl) || { echo "check-nightly-stale-index: need curl" >&2; exit 1; }
expected_nightly=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["nightly"])' "$ROOT/docs/latest.json")
[ -n "$expected_nightly" ] || { echo "check-nightly-stale-index: no nightly version in latest.json" >&2; exit 1; }

mkdir -p "$tmp/shim"
cat >"$tmp/shim/curl" <<'EOF'
#!/bin/sh
url=
for a in "$@"; do
  case "$a" in http://*|https://*) url=$a ;; esac
done
case "$url" in
  */latest.json)
    printf '{"stable":"0.0.0","nightly":"%s","updated":"1970-01-01T00:00:00Z"}\n' "$EXPECTED_NIGHTLY"
    exit 0
    ;;
  */releases/download/"$EXPECTED_NIGHTLY"/*)
    printf '%s\n' "$url" >> "$REQUEST_LOG"
    exec "$REAL_CURL" "$@"
    ;;
esac
exec "$REAL_CURL" "$@"
EOF
chmod +x "$tmp/shim/curl"

export REAL_CURL="$real_curl" EXPECTED_NIGHTLY="$expected_nightly" REQUEST_LOG="$tmp/requests"
export HOME="$tmp/home" XDG_DATA_HOME="$tmp/data" REMUDA_INSTALL_DIR="$tmp/bin"
export REMUDA_CHANNEL=nightly
mkdir -p "$HOME" "$XDG_DATA_HOME" "$REMUDA_INSTALL_DIR"

PATH="$tmp/shim:$PATH" sh "$ROOT/docs/install.sh"

grep -F "/releases/download/$expected_nightly/SHA256SUMS" "$REQUEST_LOG" >/dev/null || {
	echo "check-nightly-stale-index: installer did not try the indexed version tag first" >&2
	exit 1
}

[ -x "$REMUDA_INSTALL_DIR/remuda" ] || {
  echo "check-nightly-stale-index: install did not produce $REMUDA_INSTALL_DIR/remuda" >&2
  exit 1
}
echo "ok — cached nightly index tried immutable tag $expected_nightly first"
