#!/bin/sh
# Reference only — this script does not invoke docker itself. It documents
# the exact commands the calling session runs by hand, because env-var
# redirection (HOME/XDG_DATA_HOME/REMUDA_INSTALL_DIR, see check-cold-install.sh)
# never leaves the host machine, and this host already has a real `remuda`
# binary on PATH from prior use — a container built from scratch is the only
# way to get an actually clean machine to test docs/install.sh against.
#
# Run from the repo root. Three steps, in order:
#
# 1. Build the image (context must be the repo root, not scripts/cold-room,
#    because the Dockerfile COPYs docs/install.sh and scripts/*.sh from there):
#
#      docker build -t remuda-cold-room:dry-fit -f scripts/cold-room/Dockerfile .
#
# 2. Negative control — confirm the fresh container has no remuda on PATH
#    before install.sh ever runs:
#
#      docker run --rm remuda-cold-room:dry-fit sh /opt/remuda-repo/scripts/check-clean-machine.sh
#
# 3. Install (nightly channel — no stable release exists yet, see
#    check-cold-install.sh's header) and read back the installed binary from
#    inside the same container invocation, rather than trusting install.sh's
#    own exit code/message as the observation:
#
#      docker run --rm remuda-cold-room:dry-fit sh -c 'export REMUDA_INSTALL_DIR=/root/.local/bin REMUDA_CHANNEL=nightly; sh /opt/remuda-repo/docs/install.sh; if test -x "$REMUDA_INSTALL_DIR/remuda"; then echo "READBACK: binary present and executable at $REMUDA_INSTALL_DIR/remuda"; "$REMUDA_INSTALL_DIR/remuda" --version 2>&1 || echo "READBACK: --version failed or unsupported, that is fine, presence+executable bit is the observation"; else echo "READBACK: no binary at $REMUDA_INSTALL_DIR/remuda -- install did not land the artifact" >&2; exit 1; fi'
#
# (The original draft used a single && chain, which let the final `||`
# fallback swallow a genuine missing-binary failure as "that's fine" --
# fixed so absence is a distinct, non-zero-exit outcome from "--version
# just isn't supported.")
#
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
echo "cold-room/run.sh is documentation only — see the comments in $0 for the three commands to run by hand." >&2
