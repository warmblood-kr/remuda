#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sh "$ROOT/scripts/check-no-shell-lua.sh"

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT HUP INT TERM
mkdir -p "$fixture/scripts" "$fixture/src"
cp "$ROOT/scripts/check-no-shell-lua.sh" "$fixture/scripts/"
cat >"$fixture/scripts/no-shell-lua-allowlist.txt" <<'EOF'
src/frozen.lua|1
EOF
cat >"$fixture/src/frozen.lua" <<'EOF'
os.execute("existing allowlisted call")
EOF
cat >"$fixture/src/new.lua" <<'EOF'
io . popen ("new prohibited call")
os [ "execute" ] ("another new prohibited call")
os . execute ("spacing variant")
io["popen"]("bracketed popen")
EOF
git -C "$fixture" init -q
git -C "$fixture" add .

if sh "$fixture/scripts/check-no-shell-lua.sh" "$fixture" >"$fixture/red.log" 2>&1; then
  echo 'guard accepted a new io.popen call' >&2
  exit 1
fi
if ! grep -F 'unapproved shell call: src/new.lua' "$fixture/red.log" >/dev/null; then
  cat "$fixture/red.log" >&2
  echo 'guard failed without identifying the new shell call' >&2
  exit 1
fi

cat >"$fixture/src/new.lua" <<'EOF'
return true
EOF
cat >"$fixture/src/frozen.lua" <<'EOF'
return true
EOF
git -C "$fixture" add .
if sh "$fixture/scripts/check-no-shell-lua.sh" "$fixture" >"$fixture/stale.log" 2>&1; then
  echo 'guard accepted a stale allowlist entry' >&2
  exit 1
fi
if ! grep -F 'stale shell-call allowlist entry: src/frozen.lua' "$fixture/stale.log" >/dev/null; then
  cat "$fixture/stale.log" >&2
  echo 'guard failed without identifying the stale allowlist entry' >&2
  exit 1
fi

echo 'ok - new shell calls fail and removed calls must leave the allowlist'
