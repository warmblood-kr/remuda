#!/bin/sh
# Negative control: check-cold-install.sh only redirects HOME/XDG_DATA_HOME/
# REMUDA_INSTALL_DIR, which proves nothing about a machine that already has a
# real `remuda` binary sitting on PATH outside those redirected dirs (as this
# dev host does, at ~/.local/bin/remuda from prior real use). This script's
# only job is to assert that isn't the case — run it inside whatever you're
# calling "a clean machine" before trusting check-cold-install.sh's result.
#
# Run it yourself:  scripts/check-clean-machine.sh
set -eu

if command -v remuda >/dev/null 2>&1; then
	path=$(command -v remuda)
	echo "check-clean-machine: remuda already resolves on PATH at $path (command -v remuda) — this is not a clean machine, and no amount of HOME/XDG_DATA_HOME/REMUDA_INSTALL_DIR redirection detects this" >&2
	exit 1
fi

echo "ok — no remuda binary resolves on PATH"
