#!/bin/sh
# Contract gate: this must pass before the implementation can be merged.
set -eu
cd "$(dirname "$0")/.."
if rg -n 'PendingImplementation|todo!|unimplemented!' core/src/session_enrichment.rs; then
  echo "enrichment contract is still pending; do not merge or wire it to users" >&2
  exit 1
fi
