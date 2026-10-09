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
profdata_name=${profdata:-not found}
echo "llvm-profdata: ${profdata_name##*/}"

target=$(cd "$dir" 2>/dev/null && pwd -P || true)

profile_signature() {
  name=$1
  name=${name%.profraw}
  name=${name%_[0-9]*}
  printf '%s\n' "${name##*-}"
}

print_target_processes() {
  [ -n "$target" ] || return 0
  echo "== processes with executable or cwd under $dir (pid ppid cmdline)"
  for proc in /proc/[0-9]*; do
    [ -d "$proc" ] || continue
    exe=$(readlink "$proc/exe" 2>/dev/null || true)
    cwd=$(readlink "$proc/cwd" 2>/dev/null || true)
    case "$exe:$cwd" in
      "$target"/*:*|*:"$target"/*|"$target":*|*:"$target") ;;
      *) continue ;;
    esac
    pid=${proc##*/}
    ppid=$(awk '/^PPid:/ {print $2; exit}' "$proc/status" 2>/dev/null)
    cmd=$(tr '\000' ' ' < "$proc/cmdline" 2>/dev/null | cut -c1-240)
    [ -n "$cmd" ] || cmd='[exited or unavailable]'
    if [ -n "${HOME:-}" ]; then cmd=$(printf '%s' "$cmd" | sed "s|$HOME|~|g"); fi
    printf '  %s %s %s\n' "$pid" "${ppid:-?}" "$cmd"
  done
}

listing=$(for f in "$dir"/*.profraw; do
  [ -e "$f" ] && echo "$(size_mtime "$f") ${f##*/}"
done)
echo "== $(printf '%s\n' "$listing" | grep -c .) profiles; newest (size, mtime)"
printf '%s\n' "$listing" | sort -k2,2 -rn | head -n 15

# The trailing %m value identifies the instrumented binary.
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
    echo "   first 256 bytes (hex):"
    if command -v xxd >/dev/null 2>&1; then
      t 5 xxd -l 256 "$dir/$name" | sed 's/^/      /'
    else
      t 5 od -Ax -tx1z -N 256 -v "$dir/$name" | sed 's/^/      /'
    fi
    sig=$(profile_signature "$name")
    sibling=
    for candidate in "$dir"/*.profraw; do
      [ -f "$candidate" ] || continue
      [ "${candidate##*/}" = "$name" ] && continue
      [ "$(profile_signature "${candidate##*/}")" = "$sig" ] || continue
      if t 20 "$profdata" merge -o /dev/null "$candidate" >/dev/null 2>&1; then
        sibling=$candidate
        break
      fi
    done
    if [ -n "$sibling" ]; then
      echo "   intact sibling ${sibling##*/} (same signature $sig), first 256 bytes (hex):"
      if command -v xxd >/dev/null 2>&1; then
        t 5 xxd -l 256 "$sibling" | sed 's/^/      /'
      else
        t 5 od -Ax -tx1z -N 256 -v "$sibling" | sed 's/^/      /'
      fi
    else
      echo "   intact sibling: none found for signature $sig"
    fi
    print_target_processes
  fi
done
exit 0
