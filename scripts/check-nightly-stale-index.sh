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

# A delayed workflow_dispatch/re-run must not move the public pointer back to
# an older version after a newer publisher has already advanced it.
cat >"$tmp/latest.json" <<'EOF'
{"stable":"1.0.0","nightly":"0.1.0-nightly.20260929120000.abcdef1","updated":"old"}
EOF
stale_result=$(python3 "$ROOT/scripts/latest-index.py" "$tmp/latest.json" nightly \
	0.1.0-nightly.20260928120000.abcdef1 2>&1)
case "$stale_result" in
*"skipping stale candidate"*"publish=false"*) ;;
*) echo "check-nightly-stale-index: stale candidate was not skipped: $stale_result" >&2; exit 1 ;;
esac
grep -F '"nightly":"0.1.0-nightly.20260929120000.abcdef1"' "$tmp/latest.json" >/dev/null || {
	echo "check-nightly-stale-index: stale candidate moved latest.json backwards" >&2
	exit 1
}
echo "ok — stale nightly candidate leaves the newer latest.json pointer intact"

# Match native/src/dist.rs numeric ordering: 0.10.0 outranks 0.9.0.
printf '{"stable":"0.9.0","nightly":"0.1.0-nightly.20260929120000.abcdef1"}\n' \
	>"$tmp/numeric.json"
numeric_result=$(python3 "$ROOT/scripts/latest-index.py" "$tmp/numeric.json" stable 0.10.0)
[ "$numeric_result" = publish=true ] && grep -F '"stable": "0.10.0"' "$tmp/numeric.json" >/dev/null || {
	echo "check-nightly-stale-index: version ordering did not rank 0.10.0 above 0.9.0" >&2
	exit 1
}
echo "ok — nightly index ordering compares numeric version components"

# latest.json is public input: a malformed version must be rejected before it
# can become a release-download URL component.
if PATH="$tmp/shim:$PATH" EXPECTED_NIGHTLY='../../untrusted' \
	REQUEST_LOG="$tmp/malicious-requests" sh "$ROOT/docs/install.sh" \
	>"$tmp/malicious-output" 2>&1; then
	echo "check-nightly-stale-index: installer accepted a malformed indexed version" >&2
	exit 1
fi
grep -F 'invalid '\''nightly'\'' version in' "$tmp/malicious-output" >/dev/null || {
	cat "$tmp/malicious-output" >&2
	echo "check-nightly-stale-index: malformed version was not refused clearly" >&2
	exit 1
}
[ ! -s "$tmp/malicious-requests" ] || {
	echo "check-nightly-stale-index: malformed version was used in a release URL" >&2
	exit 1
}
echo "ok — malformed indexed version is refused before a release URL is requested"
