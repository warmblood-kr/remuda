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

	# The install dir is a fresh temp dir, so it is not on PATH: the installer
	# must print the command that fixes that (#395).
	set +e
	out=$(sh "$ROOT/docs/install.sh" 2>&1)
	status=$?
	set -e
	if [ "$status" -ne 0 ] || [ ! -x "$REMUDA_INSTALL_DIR/remuda" ]; then
		rm -rf "$tmp"
		echo "check-cold-install: REMUDA_CHANNEL=nightly did not produce $REMUDA_INSTALL_DIR/remuda: $out" >&2
		exit 1
	fi
	case "$out" in
	*"Next: export PATH=\"$REMUDA_INSTALL_DIR:\$PATH\""*) ;;
	*)
		rm -rf "$tmp"
		echo "check-cold-install: an install dir that is not on PATH got no 'Next: export PATH=...' line: $out" >&2
		exit 1
		;;
	esac

	# With the dir on PATH there is nothing to fix and nothing to say.
	set +e
	out=$(PATH="$REMUDA_INSTALL_DIR:$PATH" sh "$ROOT/docs/install.sh" 2>&1)
	status=$?
	set -e
	rm -rf "$tmp"
	if [ "$status" -ne 0 ]; then
		echo "check-cold-install: the second nightly install failed: $out" >&2
		exit 1
	fi
	case "$out" in
	*"export PATH"* | *"not on your PATH"*)
		echo "check-cold-install: an install dir already on PATH still got a PATH line: $out" >&2
		exit 1
		;;
	esac
}

check_stable_fails
check_nightly_succeeds
echo "ok — cold stable install fails with an actionable message, REMUDA_CHANNEL=nightly still installs"
