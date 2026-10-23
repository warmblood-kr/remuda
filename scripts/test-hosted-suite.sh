#!/bin/sh
# Runs the native test suite with every new session host-backed (#713), then
# checks that the run left no session host behind. Hosts outlive their daemon
# by design, so a test that ends without closing its sessions leaks one; a few
# hundred leaks use up the machine's pseudo-terminals.
#
# A run's hosts are the processes whose argv[0] is this checkout's own host
# binary (target/debug or target/debug/deps). Many tests pick their own /tmp
# paths, so TMPDIR cannot tell them apart. The run refuses to start while any
# such host is alive; any alive at the end are listed, stopped, and fail it.
#
# Cleanup rule: never pkill/killall by pattern, however anchored. Enumerate
# with ps, check that each argv names this checkout's host binary, then kill
# those exact pids one by one.
#
# Tests listed in scripts/hosted-suite-known-differences.txt are skipped:
# they check behaviour hosted sessions deliberately do not have.
#
# Usage: scripts/test-hosted-suite.sh [TEST_NAME_FILTER...]
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
HOST="$ROOT/target/debug/remuda-session-host"
TEST_HOST="$ROOT/target/debug/deps/remuda-session-host"

hosts_of_run() {
    ps -A -o pid= -o args= | while read -r pid exe verb rest; do
        case "$exe" in
        "$HOST" | "$TEST_HOST") [ "$verb" = run-reserved ] && echo "$pid" ;;
        esac
    done
}

cd "$ROOT" || exit 2
before=$(hosts_of_run)
if [ -n "$before" ]; then
    echo "test-hosted-suite: hosts of this checkout are already running; stop them first:" >&2
    echo "$before" >&2
    exit 2
fi
cargo build --bins || exit 2
# Test binaries look for the host next to themselves. Remove first: on macOS,
# copying over an existing binary keeps a stale signature and it is killed.
rm -f "$TEST_HOST"
cp "$HOST" "$TEST_HOST" || exit 2

# Short base: a Unix socket path must fit in sun_path (104 bytes on macOS).
run=$(mktemp -d /tmp/rh.XXXXXX)
for name in $(sed -e 's/#.*//' "$ROOT/scripts/hosted-suite-known-differences.txt"); do
    set -- "$@" --skip "$name"
done
TMPDIR="$run" REMUDA_EXPERIMENTAL_SESSION_HOST=1 \
    cargo test -p remuda-native --tests --no-fail-fast -- "$@"
status=$?

leaked=$(hosts_of_run)
if [ -n "$leaked" ]; then
    count=$(echo "$leaked" | wc -l | tr -d ' ')
    echo "test-hosted-suite: $count session host(s) outlived the run:" >&2
    for pid in $leaked; do
        ps -o pid= -o args= -p "$pid" >&2
        kill "$pid" 2>/dev/null
    done
    sleep 2
    # Re-verify before SIGKILL: a pid that ended may have been reused.
    for pid in $(hosts_of_run); do
        kill -9 "$pid" 2>/dev/null
    done
    [ "$status" -eq 0 ] && status=1
fi
chmod -R u+w "$run" 2>/dev/null
rm -rf "$run"
exit "$status"
