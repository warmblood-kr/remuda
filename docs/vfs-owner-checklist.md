# #778 owner checklist (about 10 minutes)

The four human-verified items in #778's definition of done, as commands
and the output to expect. Everything runs in a scratch home with a private
daemon name, so your own daemon and cluster files are not touched. Unix.

## Setup (1 minute)

Start building step 3's older remuda (schema 2, the commit before #826)
first; it compiles while you do steps 1 and 2. `REPO` is your clone:

```sh
REPO=~/remuda; OLD=/tmp/remuda-old/target/debug/remuda
git -C "$REPO" worktree add /tmp/remuda-old 30a87085~1
cargo build --manifest-path /tmp/remuda-old/Cargo.toml --bin remuda &
```

Then, in the same shell, point remuda at a scratch home:

```sh
T=$(mktemp -d /tmp/vfscheck.XXXX); db=$T/state/remuda/cluster/vfs/index.db
export HOME=$T/home XDG_STATE_HOME=$T/state XDG_CONFIG_HOME=$T/config
export XDG_DATA_HOME=$T/data REMUDA_RUNTIME_DIR=$T/rt REMUDA_NO_UPDATE_CHECK=1
mkdir -m 700 "$HOME" "$REMUDA_RUNTIME_DIR"
remuda -s check cluster init --no-listen      # prints Node: and Fingerprint:
remuda -s check stop
```

## 1. No daemon: one line, non-zero, no index touched

```sh
echo hi | remuda -s check vfs put - remuda:///x.txt; echo "exit $?"
find "$T" -name 'index.db*'
```

Expect exactly `remuda: vfs put needs the remuda daemon, and none is
running for "check"; start it with ...`, then `exit 1`, and `find` prints
nothing.

## 2. Reload: writes before, during and after all land; one writer

```sh
remuda -s check -e 1 >/dev/null
echo before | remuda -s check vfs put - remuda:///before.txt
sqlite3 -readonly "$db" "select value from meta where key='writer'"
(for i in 1 2 3 4 5 6; do echo $i | remuda -s check vfs put - remuda:///during-$i.txt; sleep 0.3; done) &
remuda -s check reload; wait
echo after | remuda -s check vfs put - remuda:///after.txt
remuda -s check vfs ls remuda:///
sqlite3 -readonly "$db" "select value from meta where key='writer'"
pgrep -fl 'remuda -s check daemon'
```

Expect `reloaded: BUILD now serves this socket`, every put says
`Stored ...`, and `ls` lists all 8 files with versions 1-8 and no gaps. The
writer line keeps the socket but its token changes across the reload (the
new daemon took the write claim), and `pgrep` shows one daemon.

## 3. Downgrade, followed once

The steps are in [vfs.md](vfs.md#downgrading-remuda). Run them:

```sh
remuda -s check vfs index-downgrade 2          # "Would back up index.db, then undo schema steps 3 (conflicts)."
remuda -s check vfs index-downgrade 2 --yes    # "index.db is now at schema 2 (was 3) ..."
remuda -s check stop
$OLD -s check -e 1 >/dev/null; $OLD -s check vfs ls remuda:/// | wc -l   # 8
echo old | $OLD -s check vfs put - remuda:///on-old.txt
$OLD -s check stop
remuda -s check -e 1 >/dev/null; remuda -s check vfs ls remuda:/// | wc -l   # 9
remuda -s check stop
```

Expect the older build to list all 8 files and accept a write, and the
current build to upgrade the index again and list 9. A `.bak` copy sits
next to `index.db`.

## 4. Defaults you agreed to (read, 2 minutes)

- One database per mod, in the mod's own namespace, with limits as
  changeable defaults: [lua.md, Mod databases](lua.md#mod-databases-remudadb).
- Lua writes the VFS only through `remuda.vfs.put` and `remuda.vfs.rm`:
  [lua-reference](lua-reference.html), `vfs.put`.
- No auto-start: step 1 above.
- VFS limits and settings: [vfs.md, Limits](vfs.md#limits) and
  [Settings](vfs.md#settings-remuda-vfs-config).

## Clean up

```sh
rm -rf "$T"; git -C "$REPO" worktree remove --force /tmp/remuda-old
```
