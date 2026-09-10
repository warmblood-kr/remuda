#!/bin/sh
# Regression test for a live defect: with no stable release ever published,
# latest.json carries "stable":"0.0.0" — a placeholder, not a version. A
# cold install (no prior channel file, no REMUDA_CHANNEL) must fail with a
# clear, actionable message that names the nightly workaround verbatim, not
# a raw 404 from a constructed tag like v0.0.0. It must NOT silently install
# nightly instead — that's a policy call for later, not this script's job.
# REMUDA_CHANNEL=nightly, set explicitly, must still install cleanly.
#
# Run it yourself:  scripts/check-cold-install.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)

check_stable_fails() {
	tmp=$(mktemp -d)
	export HOME="$tmp/home" XDG_DATA_HOME="$tmp/data" REMUDA_INSTALL_DIR="$tmp/bin"
	unset REMUDA_CHANNEL
	mkdir -p "$HOME" "$XDG_DATA_HOME" "$REMUDA_INSTALL_DIR"

	set +e
	out=$(sh "$ROOT/docs/install.sh" 2>&1)
	status=$?
	set -e

	if [ "$status" -eq 0 ]; then
		rm -rf "$tmp"
		echo "check-cold-install: expected a cold stable install to fail (no stable release published), it succeeded" >&2
		exit 1
	fi
	case "$out" in
	*REMUDA_CHANNEL=nightly*) ;;
	*)
		rm -rf "$tmp"
		echo "check-cold-install: failure message does not name the nightly workaround: $out" >&2
		exit 1
		;;
	esac
	if [ -x "$REMUDA_INSTALL_DIR/remuda" ]; then
		rm -rf "$tmp"
		echo "check-cold-install: a binary was installed despite the expected failure" >&2
		exit 1
	fi
	rm -rf "$tmp"
}

check_nightly_succeeds() {
	tmp=$(mktemp -d)
	export HOME="$tmp/home" XDG_DATA_HOME="$tmp/data" REMUDA_INSTALL_DIR="$tmp/bin"
	export REMUDA_CHANNEL=nightly
	mkdir -p "$HOME" "$XDG_DATA_HOME" "$REMUDA_INSTALL_DIR"

	sh "$ROOT/docs/install.sh"
	if [ ! -x "$REMUDA_INSTALL_DIR/remuda" ]; then
		rm -rf "$tmp"
		echo "check-cold-install: REMUDA_CHANNEL=nightly did not produce $REMUDA_INSTALL_DIR/remuda" >&2
		exit 1
	fi
	rm -rf "$tmp"
}

check_stable_fails
check_nightly_succeeds
echo "ok — cold stable install fails with an actionable message, REMUDA_CHANNEL=nightly still installs"
