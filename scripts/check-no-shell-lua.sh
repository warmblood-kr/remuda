#!/bin/sh
# Freeze existing Lua shell calls; additions and stale exceptions fail CI.
# This is a per-line text scan of git-tracked *.lua; comments may be false positives.
# It misses table aliases (`local o = os`), package.loaded["os"], _G/_ENV lookups,
# load(), split references, and package.loadlib/ffi; code review covers those.
set -eu

ROOT=${1:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}
ROOT=$(CDPATH= cd -- "$ROOT" && pwd)
ALLOWLIST=$ROOT/scripts/no-shell-lua-allowlist.txt
[ -f "$ALLOWLIST" ] || { echo "missing Lua shell-call allowlist: $ALLOWLIST" >&2; exit 2; }

BAD=$(mktemp)
FILES=$(mktemp)
trap 'rm -f "$BAD" "$FILES"' EXIT HUP INT TERM
git -C "$ROOT" ls-files -- '*.lua' >"$FILES"

count_calls() {
  file=$1
  [ -f "$file" ] || { echo 0; return; }
  awk '
    {
      line = $0
      calls += gsub(/os[[:space:]]*[.][[:space:]]*execute/, "", line)
      calls += gsub(/io[[:space:]]*[.][[:space:]]*popen/, "", line)
      calls += gsub(/os[[:space:]]*\[[[:space:]]*[\042\047]execute[\042\047][[:space:]]*\]/, "", line)
      calls += gsub(/io[[:space:]]*\[[[:space:]]*[\042\047]popen[\042\047][[:space:]]*\]/, "", line)
    }
    END { print calls + 0 }
  ' "$file"
}

while IFS= read -r path; do
  count=$(count_calls "$ROOT/$path")
  [ "$count" -eq 0 ] && continue
  expected=$(awk -F '|' -v path="$path" '$1 == path { print $2 }' "$ALLOWLIST")
  if [ -z "$expected" ]; then
    printf 'unapproved shell call: %s (%s occurrence(s), not allowlisted)\n' "$path" "$count" >>"$BAD"
  elif [ "$count" != "$expected" ]; then
    printf 'shell-call allowlist mismatch: %s expected %s, found %s\n' "$path" "$expected" "$count" >>"$BAD"
  fi
done <"$FILES"

while IFS='|' read -r path expected extra; do
  [ -n "$path" ] || { echo 'empty path in Lua shell-call allowlist' >&2; exit 2; }
  case "$expected" in ''|*[!0-9]*) printf 'invalid count in Lua shell-call allowlist: %s\n' "$path" >&2; exit 2 ;; esac
  [ -z "${extra:-}" ] || { printf 'invalid allowlist row: %s|%s|%s\n' "$path" "$expected" "$extra" >&2; exit 2; }
  entries=$(awk -F '|' -v path="$path" '$1 == path { n++ } END { print n + 0 }' "$ALLOWLIST")
  [ "$entries" -eq 1 ] || { printf 'duplicate Lua shell-call allowlist entry: %s\n' "$path" >>"$BAD"; continue; }
  actual=$(count_calls "$ROOT/$path")
  [ "$actual" = "$expected" ] || printf 'stale shell-call allowlist entry: %s expected %s, found %s\n' "$path" "$expected" "$actual" >>"$BAD"
done <"$ALLOWLIST"

if [ -s "$BAD" ]; then cat "$BAD" >&2; exit 1; fi
echo 'ok - Lua shell calls match the frozen allowlist'
