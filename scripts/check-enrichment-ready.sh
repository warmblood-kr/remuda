#!/bin/sh
# Contract gate: this must pass before the implementation can be merged.
set -eu
if ! command -v grep >/dev/null 2>&1; then
  printf '%s\n' 'enrichment guard: grep is required' >&2
  exit 2
fi
case "$0" in
  /*) enrichment_guard_dir=${0%/*} ;;
  */*) enrichment_guard_dir="$(pwd -P)/${0%/*}" ;;
  *) printf '%s\n' 'enrichment guard: cannot locate source root' >&2; exit 2 ;;
esac
if ! cd "$enrichment_guard_dir/.."; then
  printf '%s\n' 'enrichment guard: source root is unavailable' >&2
  exit 2
fi
for enrichment_guard_dir in core/src native/src; do
  if [ ! -d "$enrichment_guard_dir" ]; then
    printf 'enrichment guard: missing directory %s\n' "$enrichment_guard_dir" >&2
    exit 2
  fi
done
for enrichment_guard_file in core/src/session_enrichment.rs native/src/lib.rs; do
  if [ ! -f "$enrichment_guard_file" ] || [ ! -r "$enrichment_guard_file" ]; then
    printf 'enrichment guard: missing or unreadable file %s\n' "$enrichment_guard_file" >&2
    exit 2
  fi
done
if grep -r -n -E 'PendingImplementation|todo!|unimplemented!' core/src native/src; then
  printf '%s\n' 'enrichment contract is still pending; do not merge or wire it to users' >&2
  exit 1
else
  enrichment_guard_rc=$?
  if [ "$enrichment_guard_rc" -ne 1 ]; then
    printf 'enrichment guard: source scan failed (grep exit %s)\n' "$enrichment_guard_rc" >&2
    exit 2
  fi
fi
