#!/usr/bin/env bash
# Run remuda-butler's live-reload contract against THIS core checkout.
#
# Builds the core here, puts it first on PATH, and runs remuda-butler's
# tests/live_reload.sh twice: with an explicit private daemon and with a
# CLI-auto-started one (AUTOSTART=1). Everything runs on a private -s server
# with scratch HOME/XDG dirs; the operator's default daemon is never touched.
#
#   scripts/check-butler-contract.sh                  # clones remuda-butler main
#   BUTLER_REPO=~/src/remuda-butler scripts/check-butler-contract.sh
#
# Needs: cargo, git, bash, tar, pgrep/pkill, python3 (Butler's relay helper).
# BUTLER_REPO must have full history: OLD_REF is archived from it.
set -euo pipefail

CORE=$(cd "$(dirname "$0")/.." && pwd)
BUTLER_URL=${BUTLER_URL:-https://github.com/warmblood-kr/remuda-butler.git}
BUTLER_REF=${BUTLER_REF:-main}
# The last legacy (pre-lifecycle) Butler. live_reload.sh replays the
# legacy -> lifecycle upgrade, so its baseline must be a legacy install;
# its own default (origin/main) is already a lifecycle Butler.
OLD_REF=${OLD_REF:-8950e51^}

scratch=$(mktemp -d /tmp/butler-contract.XXXXXX)
trap 'rm -rf "$scratch"' EXIT

if [[ -z ${BUTLER_REPO:-} ]]; then
  BUTLER_REPO=$scratch/remuda-butler
  git clone --quiet "$BUTLER_URL" "$BUTLER_REPO"
  git -C "$BUTLER_REPO" checkout --quiet "$BUTLER_REF"
fi
git -C "$BUTLER_REPO" rev-parse --verify --quiet "$OLD_REF^{commit}" >/dev/null \
  || { echo "BUTLER_REPO lacks $OLD_REF (shallow clone?)" >&2; exit 2; }

cargo build --quiet --manifest-path "$CORE/Cargo.toml" -p remuda-native --bin remuda
export PATH="$CORE/target/debug:$PATH"
export REMUDA_NO_UPDATE_CHECK=1
unset REMUDA_SERVER
echo "core $(git -C "$CORE" rev-parse --short HEAD), butler $(git -C "$BUTLER_REPO" rev-parse --short HEAD), OLD_REF $OLD_REF"
echo "remuda: $(command -v remuda)"

# A leaked stand-in agent can outlive its run (that leak is what this gate
# catches); kill each run's new ones so none lingers into the next.
stand_ins() { pgrep -fx 'sleep [1-9][0-9]{4}[12]' | sort || true; }
run() {
  local before
  before=$(stand_ins)
  "$@" || status=1
  comm -13 <(echo "$before") <(stand_ins) | xargs kill 2>/dev/null || true
}

status=0
echo "=== explicit daemon"
run "$BUTLER_REPO/tests/live_reload.sh" "$OLD_REF"
echo "=== AUTOSTART=1"
run env AUTOSTART=1 "$BUTLER_REPO/tests/live_reload.sh" "$OLD_REF"
if [[ $status -eq 0 ]]; then echo "butler contract: PASS"; else echo "butler contract: FAIL" >&2; fi
exit "$status"
