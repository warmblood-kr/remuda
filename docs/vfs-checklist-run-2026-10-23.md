# #778 owner checklist: run on 2026-10-23

The sqlite-core session ran [vfs-owner-checklist.md](vfs-owner-checklist.md)
end to end in a scratch home (`-s check`, its own HOME, XDG dirs and
runtime dir). The owner's own daemon and home were not touched.

| # | Item | Result |
|---|------|--------|
| 1 | No daemon: one-line error, exit 1, no index.db created | PASS |
| 2 | Reload: 8 puts before/during/after all stored (v1-v8, no gaps), writer token changed, one daemon | PASS |
| 3 | Downgrade to schema 2, older build lists 8 and writes 1, current build upgrades and lists 9, `.bak` kept | PASS |
| 4 | Defaults to read (below) | needs owner OK |

Builds: current = main `de27930e`; older = `2fcaddb5` (`30a87085~1`,
schema 2, before #826). `$T` is the scratch home; `$OLD` is the older build.

## 4. Defaults for the owner to OK

- One database per mod, in the mod's own namespace, limits are changeable
  defaults: [lua.md, Mod databases](lua.md#mod-databases-remudadb).
- Lua writes the VFS only through `remuda.vfs.put` / `remuda.vfs.rm`:
  [lua-reference](lua-reference.html), `vfs.put`.
- No auto-start: a VFS command with no daemon fails in one line (item 1).
- VFS limits and settings: [vfs.md, Limits](vfs.md#limits) and
  [Settings](vfs.md#settings-remuda-vfs-config).

## Setup

```text
$ remuda --version
remuda 0.1.0+git.de27930e8511
$ $OLD --version
remuda 0.1.0+git.2fcaddb5a7f8
$ remuda -s check cluster init --no-listen
remuda: started a daemon for "check": none was running (sessions of an earlier daemon, if any, are gone)
Cluster initialized
Node: node-rteetsol
Fingerprint: SHA256:RteEtsOlNSAsvd7zRO0l1HAGpy2NcuYvtg6u9Rr0ddc
Listener off (--no-listen)
Next: run `remuda cluster invite` here, then run its printed `remuda cluster join ...` command on the other node.
$ remuda -s check stop
remuda: stopped the daemon for "check" — the next command starts a fresh one
```

## 1. No daemon

```text
$ echo hi | remuda -s check vfs put - remuda:///x.txt; echo "exit $?"
remuda: vfs put needs the remuda daemon, and none is running for "check"; start it with `remuda run ...` or `remuda -e 1`, then retry
exit 1
$ find "$T" -name "index.db*"
(no output: no index.db was created)
```

## 2. Reload

```text
$ remuda -s check -e 1 >/dev/null
remuda: started a daemon for "check": none was running (sessions of an earlier daemon, if any, are gone)
$ echo before | remuda -s check vfs put - remuda:///before.txt
Stored remuda://default/before.txt (version 1, sha256 9160d4be34c8, 7 bytes, 3 copies wanted).
$ sqlite3 -readonly "$db" "select value from meta where key='writer'"
$T/rt/remuda/check.sock	662fa4f99f5b4436f452353d36c88fcc
$ (for i in 1..6; put during-$i; sleep 0.3) & remuda -s check reload; wait
Stored remuda://default/during-1.txt (version 2, sha256 4355a46b19d3, 2 bytes, 3 copies wanted).
reloaded: 0.1.0+git.de27930e8511 now serves this socket
daemon now 0.1.0+git.de27930e8511
sessions: all 0 host-backed listed
init.lua: loaded
Stored remuda://default/during-2.txt (version 3, sha256 53c234e5e847, 2 bytes, 3 copies wanted).
Stored remuda://default/during-3.txt (version 4, sha256 1121cfccd591, 2 bytes, 3 copies wanted).
Stored remuda://default/during-4.txt (version 5, sha256 7de1555df0c2, 2 bytes, 3 copies wanted).
Stored remuda://default/during-5.txt (version 6, sha256 f0b5c2c2211c, 2 bytes, 3 copies wanted).
Stored remuda://default/during-6.txt (version 7, sha256 06e9d52c1720, 2 bytes, 3 copies wanted).
$ echo after | remuda -s check vfs put - remuda:///after.txt
Stored remuda://default/after.txt (version 8, sha256 7b9a72466d39, 6 bytes, 3 copies wanted).
$ remuda -s check vfs ls remuda:///
         6  v8     7b9a72466d39  after.txt
         7  v1     9160d4be34c8  before.txt
         2  v2     4355a46b19d3  during-1.txt
         2  v3     53c234e5e847  during-2.txt
         2  v4     1121cfccd591  during-3.txt
         2  v5     7de1555df0c2  during-4.txt
         2  v6     f0b5c2c2211c  during-5.txt
         2  v7     06e9d52c1720  during-6.txt
$ sqlite3 -readonly "$db" "select value from meta where key='writer'"
$T/rt/remuda/check.sock	a87df1e5b200f1807eba44268d2c937b
$ pgrep -fl 'remuda -s check daemon'
95549 remuda -s check daemon
```

during-1 went to the old daemon, during-2..6 to the reloaded one.

## 3. Downgrade round trip

```text
$ remuda -s check vfs index-downgrade 2
Would back up index.db, then undo schema steps 3 (conflicts). Run again with --yes.
$ remuda -s check vfs index-downgrade 2 --yes
Backed up to $T/state/remuda/cluster/vfs/index.db.v3-1792740038.bak.
index.db is now at schema 2 (was 3). This daemon no longer uses the VFS: stop it and start the older remuda.
$ remuda -s check stop
remuda: stopped the daemon for "check" — the next command starts a fresh one
$ $OLD -s check -e 1 >/dev/null; $OLD -s check vfs ls remuda:/// | wc -l
remuda: started a daemon for "check": none was running (sessions of an earlier daemon, if any, are gone)
       8
$ echo old | $OLD -s check vfs put - remuda:///on-old.txt
Stored remuda://default/on-old.txt (version 9, sha256 01d09d19c213, 4 bytes, 3 copies wanted).
$ $OLD -s check stop
remuda: stopped the daemon for "check" — the next command starts a fresh one
$ remuda -s check -e 1 >/dev/null; remuda -s check vfs ls remuda:/// | wc -l
remuda: started a daemon for "check": none was running (sessions of an earlier daemon, if any, are gone)
       9
$ ls "$(dirname "$db")" | grep -c "\.bak"
1
$ remuda -s check stop
remuda: stopped the daemon for "check" — the next command starts a fresh one
no check daemon left
```

