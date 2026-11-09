#!/bin/sh
# Exercise the real enrichment guard in disposable source trees, including
# missing tools and scan errors. POSIX sh only; the core tree forbids Python.
set -eu

gate_src=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/check-enrichment-ready.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/enrichment-guard.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
root="$tmp/repo with spaces"
mkdir -p "$root/scripts" "$root/core/src" "$root/native/src" "$root/bin"
cp "$gate_src" "$root/scripts/check-enrichment-ready.sh"
printf '%s\n' 'pub struct Ready;' >"$root/core/src/session_enrichment.rs"
printf '%s\n' 'pub fn ready() {}' >"$root/native/src/lib.rs"
grep_bin=$(command -v grep)
sh_bin=$(command -v sh)
ln -s "$grep_bin" "$root/bin/grep"

fail() {
  printf 'FAIL %s\n' "$1" >&2
  if [ -s "$tmp/out" ]; then cat "$tmp/out" >&2; fi
  if [ -s "$tmp/err" ]; then cat "$tmp/err" >&2; fi
  exit 1
}

run_gate() {
  expected=$1
  path=${2:-$root/bin}
  set +e
  PATH="$path" "$sh_bin" "$root/scripts/check-enrichment-ready.sh" >"$tmp/out" 2>"$tmp/err"
  rc=$?
  set -e
  if [ "$rc" -ne "$expected" ]; then
    fail "expected exit $expected, got $rc"
  fi
}

expect_stderr() {
  needle=$1
  if ! grep -F "$needle" "$tmp/err" >/dev/null 2>&1; then
    fail "stderr did not contain: $needle"
  fi
}

run_gate 0
printf '%s\n' 'PASS clean sources pass without ripgrep or dirname'

printf '%s\n' 'PendingImplementation' >"$root/core/src/session_enrichment.rs"
run_gate 1
printf '%s\n' 'PASS core placeholder blocks'
printf '%s\n' 'pub struct Ready;' >"$root/core/src/session_enrichment.rs"

printf '%s\n' 'PendingImplementation' >"$root/native/src/moved.rs"
run_gate 1
printf '%s\n' 'PASS moved native placeholder blocks'
rm -f "$root/native/src/moved.rs"

mkdir -p "$root/core/src/moved"
printf '%s\n' 'PendingImplementation' >"$root/core/src/moved/pending.rs"
run_gate 1
printf '%s\n' 'PASS moved core placeholder blocks'
rm -rf "$root/core/src/moved"

mkdir -p "$root/native/src/nested"
for marker in 'todo!()' 'unimplemented!()'; do
  printf '%s\n' "$marker" >"$root/native/src/nested/pending.rs"
  run_gate 1
done
printf '%s\n' 'PASS nested todo/unimplemented block'
rm -rf "$root/native/src/nested"

mkdir -p "$root/empty-path"
run_gate 2 "$root/empty-path"
expect_stderr 'grep is required'
printf '%s\n' 'PASS missing grep fails closed'

rm -f "$root/bin/grep"
printf '%s\n' '#!/bin/sh' 'exit 2' >"$root/bin/grep"
chmod +x "$root/bin/grep"
run_gate 2
expect_stderr 'scan failed'
printf '%s\n' 'PASS scan error fails closed'

for directory in core/src native/src; do
  mv "$root/$directory" "$root/saved"
  run_gate 2
  mv "$root/saved" "$root/$directory"
done
printf '%s\n' 'PASS missing directories fail closed'

for name in core/src/session_enrichment.rs native/src/lib.rs; do
  saved="$tmp/saved-file"
  cp "$root/$name" "$saved"
  rm -f "$root/$name"
  run_gate 2
  cp "$saved" "$root/$name"
done
printf '%s\n' 'PASS missing required files fail closed'

printf '%s\n' 'enrichment guard tests: PASS'
