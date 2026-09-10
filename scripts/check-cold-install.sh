#!/bin/sh
# Regression test for the ALTERNATIVE to what's on main: with no stable
# release ever published, latest.json carries "stable":"0.0.0" — a
# placeholder, not a version. Under this policy, a cold install (no prior
# channel file, no REMUDA_CHANNEL) falls back to nightly instead of failing,
# prints the disclosure, and — the bug-found-in-review-and-fixed part —
# still records the REQUESTED channel ('stable'), not the one the fallback
# actually installed, so a later run keeps re-checking whether stable is
# real yet instead of being silently pinned to nightly forever.
#
# Run it yourself:  scripts/check-cold-install.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

export HOME="$tmp/home"
export XDG_DATA_HOME="$tmp/data"
export REMUDA_INSTALL_DIR="$tmp/bin"
unset REMUDA_CHANNEL
mkdir -p "$HOME" "$XDG_DATA_HOME" "$REMUDA_INSTALL_DIR"

set +e
out=$(sh "$ROOT/docs/install.sh" 2>&1)
status=$?
set -e

if [ "$status" -ne 0 ]; then
	echo "check-cold-install: expected a cold install to fall back and succeed, it failed instead: $out" >&2
	exit 1
fi
case "$out" in
*"installing nightly instead"*) ;;
*)
	echo "check-cold-install: fallback disclosure was not printed: $out" >&2
	exit 1
	;;
esac

[ -x "$REMUDA_INSTALL_DIR/remuda" ] || {
	echo "check-cold-install: cold install did not produce $REMUDA_INSTALL_DIR/remuda" >&2
	exit 1
}

# A fallback substitution is not a choice: with no REMUDA_CHANNEL, the
# requested channel is the default, 'stable' — the persisted channel file
# must say that, never whatever the fallback installed instead. Otherwise a
# user who asked for (or defaulted to) stable is silently pinned to nightly
# forever, and never re-checked once a real stable release exists.
recorded_channel=$(cat "$XDG_DATA_HOME/remuda/channel" 2>/dev/null || echo "<missing>")
[ "$recorded_channel" = stable ] || {
	echo "check-cold-install: channel file recorded '$recorded_channel', expected 'stable' — a fallback substitution must not be persisted as a choice" >&2
	exit 1
}

echo "ok — cold install (default env, no channel file, no REMUDA_CHANNEL) fell back to nightly with disclosure, channel file correctly records 'stable' as requested"
