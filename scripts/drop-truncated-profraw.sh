#!/usr/bin/env bash
# Moves aside any .profraw that llvm-profdata cannot read, so one truncated
# profile does not sink the whole coverage report (#624). A test that SIGKILLs
# a daemon while it is exiting cuts its profile short mid-write. Each file
# set aside is named on stdout; the rest are merged as usual. Always exits 0.
# Usage: drop-truncated-profraw.sh [PROFILE_DIR [LLVM_PROFDATA]]
set -u
dir=${1:-target/llvm-cov-target}
profdata=${2:-$(find "$(rustc --print sysroot)" -name llvm-profdata -type f 2>/dev/null | head -n 1)}
if [ -z "$profdata" ]; then
  echo "drop-truncated-profraw: llvm-profdata not found; nothing checked"
  exit 0
fi
aside="$dir/../unreadable-profraw"
dropped=0
for f in "$dir"/*.profraw; do
  [ -e "$f" ] || continue
  "$profdata" merge -sparse "$f" -o /dev/null >/dev/null 2>&1 && continue
  mkdir -p "$aside"
  mv -- "$f" "$aside/"
  echo "set aside unreadable profile ${f##*/} ($(wc -c < "$aside/${f##*/}" | tr -d ' ') bytes)"
  dropped=$((dropped + 1))
done
echo "drop-truncated-profraw: $dropped unreadable profile(s) set aside"
exit 0
