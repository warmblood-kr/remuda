#!/usr/bin/env bash
# Print-only evidence for #624 after a failed Coverage step: which .profraw
# llvm-profdata rejects, how it differs from intact profiles of the same
# binary, and which instrumented processes are still alive. Always exits 0.
# Usage: diagnose-profraw.sh [PROFILE_DIR [LLVM_PROFDATA]]
set -u
dir=${1:-target/llvm-cov-target}
t() { if command -v timeout >/dev/null; then timeout "$@"; else shift; "$@"; fi; }
# "SIZE MTIME" with nanoseconds: GNU stat on CI, BSD stat for a local run.
size_mtime() { t 5 stat -c '%s %.9Y' "$1" 2>/dev/null || t 5 stat -f '%z %Fm' "$1"; }

echo "now: $(date -u +%Y-%m-%dT%H:%M:%SZ) ($(t 5 date +%s.%N 2>/dev/null || date +%s))"
profdata=${2:-$(t 30 find "$(t 30 rustc --print sysroot)" -name llvm-profdata -type f 2>/dev/null | head -n 1)}
echo "llvm-profdata: ${profdata:-not found}"

echo "== live processes started from $dir"
{ t 10 ps -eo pid,ppid,etimes,args 2>/dev/null || t 10 ps -eo pid,ppid,etime,args; } |
  grep -F -- "$dir/" | grep -v -e grep -e diagnose-profraw | cut -c1-300

listing=$(for f in "$dir"/*.profraw; do
  [ -e "$f" ] && echo "$(size_mtime "$f") ${f##*/}"
done)
echo "== $(printf '%s\n' "$listing" | grep -c .) profiles; newest (size, mtime)"
printf '%s\n' "$listing" | sort -k2,2 -rn | head -n 15

# The %m signature names one binary; its intact profiles all have one size.
echo "== profiles whose size differs from the most common size for their signature"
printf '%s\n' "$listing" | awk 'NF==3 {sig=$3; sub(/_[0-9]+\.profraw$/, "", sig); sub(/^.*-/, "", sig);
  n[sig" "$1]++; f[sig" "$1]=$3} END {for (k in n) print k, n[k], f[k]}' |
  sort -k1,1 -k3,3nr | awk '$1==last {print "  odd:", $4, "size", $2, "(" $3 " like it)"; next} {last=$1}'

[ -n "$profdata" ] || exit 0
echo "== profiles llvm-profdata rejects"
printf '%s\n' "$listing" | while read -r size mtime name; do
  [ -n "${name:-}" ] || continue
  if ! out=$(t 20 "$profdata" merge -o /dev/null "$dir/$name" 2>&1); then
    echo "-- $name size $size mtime $mtime"
    printf '%s\n' "$out" | head -n 2 | sed 's/^/   /'
    # Raw v10/v11 order: Magic Version BinaryIdsSize NumData PadBefore
    # NumCounters PadAfter NumBitmapBytes PadAfterBitmap NamesSize ...
    echo "   header u64: $(t 5 od -An -t u8 -N 128 -v "$dir/$name" | tr -s ' \n' ' ')"
  fi
done
exit 0
