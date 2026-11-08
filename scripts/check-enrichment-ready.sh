#!/bin/sh
# Contract gate: this must pass before the implementation can be merged.
# Fail closed: a missing or failed search is not a clean result.
set -eu
cd "$(dirname "$0")/.."
if command -v rg >/dev/null 2>&1; then
  search() { rg -n 'PendingImplementation|todo!|unimplemented!' core/src native/src; }
elif command -v grep >/dev/null 2>&1; then
  search() { grep -rEn 'PendingImplementation|todo!|unimplemented!' core/src native/src; }
else
  echo "no search tool available; refusing to pass" >&2
  exit 1
fi
status=0
search || status=$?
if [ "$status" -eq 0 ]; then
  echo "enrichment contract still has placeholders; do not merge or wire it to users" >&2
  exit 1
fi
if [ "$status" -ne 1 ]; then
  echo "enrichment placeholder search failed (status $status); refusing to pass" >&2
  exit 1
fi
