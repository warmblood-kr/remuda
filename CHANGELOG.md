# Changelog

## Unreleased

- `remuda reload` (Unix) now checks the new daemon after the handover: it
  answers as the target build, lists every host-backed session the reload
  moved, and loaded `init.lua`. An `init.lua` that fails only in the new
  daemon, for example one edited since the old daemon started, is named and
  `reload` exits 1. The reload itself has gone through.

- VFS: only the daemon that serves the socket writes the index, also during
  `remuda reload` (#778, #713). The new daemon claims `index.db` just
  before it serves; a rollback gives it back to the old daemon. A write on
  the replaced daemon fails with `superseded` and a hint to run the command
  again. Before, both daemons could write while they overlapped.

- `remuda reload` (experimental, Unix) replaces the running daemon with the
  installed remuda without closing a host-backed session.
  - The old daemon checks the new build as for `--check`, then hands it the
    socket and its session hosts.
  - Clients keep using the same socket throughout; one waiting to be
    accepted is answered by the new daemon.
  - If the new daemon fails or stalls before it serves, it is stopped, the
    old daemon takes everything back and keeps serving, and `reload` exits
    1.
  - In-process sessions still block a reload.
  - Lua: `remuda.daemon.reloaded_from()` returns the pid of the daemon this
    one took over from, or nil after a fresh start.

- Lua and CLI: schema migrations for mod databases (#778). A mod ships
  numbered plain-SQL steps (`migrations/NAME/0001_create.up.sql`, an
  optional `.down.sql`) and opens with `remuda.db.open(name, { migrations =
  true })`. Pending steps run in one transaction, so a crash leaves the old
  schema. An edited step (`drift`) or a database newer than the mod
  (`too_new`) refuses the open. Each step's down SQL is stored in the
  database, so `remuda db downgrade MOD NAME VERSION --yes` can step back
  without the newer mod; it writes a private backup first. `remuda db
  status` and `remuda db upgrade` run in the daemon, like `remuda vfs`. A
  handle can no longer set `user_version`. The VFS `index.db` now uses the
  same runner. Its schema is unchanged (version 1), so older builds still
  open it.

- `remuda reload --check` (experimental, Unix) reports whether the installed
  remuda could take over the running daemon without losing a session. It
  starts the new build as a candidate beside the running daemon and checks
  that it can drive every session host. Then it stops the candidate and
  changes nothing. In-process sessions, and any host the new build cannot
  drive, are reported as blockers. `remuda reload` without `--check` is not
  available yet.
- Session screen: press `m` on the session list to write a line for the
  selected session in a line editor at the bottom of the screen. Nothing reaches the
  session while you type. Enter sends the whole line, with its Return, as one
  input that the daemon writes without letting other input in between. So a
  Butler notice or other injected text can no longer land inside a
  half-typed line and be submitted with it. Esc or Ctrl-\ goes back to the
  list and keeps the draft for that session; Enter or `i` still attaches
  with every key sent live, as before. Editing keys are the cluster
  composer's (`cluster.composer`), including Up/Down history.

- VFS: upgrading again after a downgrade keeps the files the older remuda
  wrote meanwhile (#778). If `index.json` changed since the last import or
  `export-index`, the daemon merges it into `index.db` again; per path the
  higher version wins. Before, the node's index missed those files until a
  peer synced them back, and a lone node missed them for good.

- Session hosts (experimental, internal): building blocks for `remuda reload`.
  A daemon can claim its session hosts again after another daemon claimed
  them, and every host it has open then refuses the other daemon at once.
  A candidate daemon can check every host read-only, without claiming. No
  command uses these yet.
- Session hosts (experimental): `remuda stop` now lets an act a host is
  already running finish and answers it, instead of hanging up on it. New
  host-backed input, resize and close are refused while the daemon drains.
  `stop` still returns at once, and hosts keep running. Before, such an act
  answered "the daemon hung up without answering" and was left for the next
  daemon to resolve.
- Session hosts (experimental): a daemon now starts only a session host of
  its own build. Before, a daemon still running after an upgrade started the
  newly installed host. `install.sh` now also keeps each host as
  `remuda-session-host-<build>` (the three most recent), and the daemon
  looks there first. If no host of its build is left, `new` is refused at
  once with a message to restart the daemon. `remuda-session-host version`
  prints the host's build.
- `remuda version --hosts` prints the daemon's build and the build of each
  host-backed session's host (experimental session hosts, #713). Hosts
  outlive daemons, so after an upgrade some sessions can still run on an
  older host; these are marked, and unreachable hosts are named. Plain
  `remuda version` is unchanged.

- Session hosts (experimental): a host now refuses input, resize, close and
  stop from a daemon whose inventory writer epoch is older than one it has
  seen (`writer-fence-v1`, error `superseded_writer`). Before, an older daemon
  that passed its store check just before a newer daemon claimed could still
  act on the host. The wire change is additive: older hosts get the plain
  `Acquire`, and older daemons are still accepted. CI now hands a hosted
  session between the base build and this build in both directions.

- Lua: `remuda.db.open(name [, opts])` gives each mod its own SQLite
  databases (#778): `open("notes")` in mod `butler` opens
  `<state>/remuda/db/butler/notes.db`. Handles have `query`, `exec`, `batch`,
  `limits` and `close`. Default limits are 64 MiB per database, 1 s per
  statement and 10,000 rows per query; a mod may ask for up to 1 GiB, 10 s
  and 100,000. `ATTACH`, `DETACH` and most PRAGMAs are refused. Outside a
  mod's own code, `open` returns `nil, "no_mod: ..."`.
- Lua: `remuda.vfs.put(uri, bytes [, replicas])` and `remuda.vfs.rm(uri)`
  write the cluster file store, with the same rules as `remuda vfs put/rm`.
  Peers are told in the background, so the Lua thread never waits on them.

- VFS: `remuda vfs` commands now run in the daemon, which owns the index
  (#778). With no daemon running, a command prints one line saying the
  daemon is needed and exits 1; it does not start one, and it does not
  touch the store. Start the daemon with `remuda run ...` or `remuda -e 1`.
  `put` still reads its source and `get` still writes its destination in
  your shell, so relative paths and stdin work as before. New:
  `remuda vfs export-index` writes `index.json` from the database for an
  older remuda; run it before a downgrade.

- VFS: the name index is now a SQLite database, `vfs/index.db` (#778). A
  write changes only its own row instead of rewriting the whole
  `index.json`, and a crash cannot lose a committed write. On first use an
  existing `index.json` is imported once and then left untouched, so an
  older remuda still finds the index as it was at the upgrade. Peers see no
  difference: the entries, the digest and the messages are the same, so
  upgraded and older nodes sync with each other as before.

- SQLite is now built into the binary (`rusqlite`, bundled; #778). It backs
  the daemon-owned VFS index and Lua
  databases. A database opened through it keeps its file, `-wal` and `-shm`
  private: 0600 in a 0700 directory on Unix, the owner-only ACL on Windows.
  The `-wal` and `-shm` files are kept when the database closes, so SQLite
  never recreates them with default permissions. A loose or symlinked file is
  refused, not used. The release build adds about 1.8 MB once SQLite is
  linked (measured on aarch64-apple-darwin).

- VFS: a node now checks a blob's hash before it serves the first chunk to a
  peer. A damaged copy is removed and refused (`corrupt`), so the peer tries
  another holder and this node's repair pass fetches a good copy. Before,
  the damage showed only as a hash mismatch at the end of the peer's pull.

- VFS: a local blob file whose size differs from its index entry (a torn
  copy, for example) now counts as missing. `stat`, `ls`, Lua `local`,
  `--wait` and the repair pass used to treat any file with the right name as
  a good copy, so repair never replaced it. A pulled copy now replaces such a
  file. Peers asking by hash alone (`BlobStat`) still get existence only.

- VFS: deleted files (tombstones) no longer count toward the 10,000-file cap.
  Before, deletes filled the cap, and once full a node silently stopped
  taking new paths from peers and diverged from them for good. The whole
  index, files and tombstones, is now capped at 30,000 entries, and a node
  pulls up to that many from each peer. Older nodes still cap the whole index
  at 10,000 and pull at most about 10,500 entries from a peer. When the index
  is full and a peer sends newer entries it cannot take, the daemon log now
  says how many it skipped and that the node now differs from its peers (at
  most once every 10 minutes).

- VFS: `remuda vfs put` no longer reports success when no peer was reached.
  It still prints `Told 0 of N peers about it.`, then says the file is stored
  on this node only and exits 75 (EX_TEMPFAIL) so a script can retry. A
  one-node cluster, with no peers to tell, still exits 0. With `--wait`, the
  wait result decides; a timed-out wait with no peer reached also exits 75.

- Install: releases now ship `remuda-session-host` beside `remuda` in every
  tarball, and `install.sh` and `install.ps1` install both as one pair. The
  pair being replaced is kept as `remuda.prev` and `remuda-session-host.prev`
  (`.exe.prev` on Windows) for rollback. Before, no release carried the host,
  so `REMUDA_EXPERIMENTAL_SESSION_HOST=1` failed with "session host binary
  not found" on any installed remuda. The daemon now also looks for
  `remuda-session-host.exe` on Windows.

- Cluster TUI: a slow or unreachable peer no longer freezes the cursor.
  Remote Input, InputLine, ResizeSession and Close now run on a background
  worker for each node, and the UI applies each reply when it arrives. Before,
  each call waited up to 1 s (8 s for a line) between drawing and reading the
  next key. A failed pane resize now waits 1 s, 2 s, 4 s and so on, up to
  30 s, before it is sent again to that session, even when the pane size
  changes. Before, its own error notice changed the size and started another
  resize at once. In keys mode, buffered keys are sent 40 ms after the first
  one even while typing continues. Before, they waited until typing paused.

- Input: `type_text` now reports `submitted` when the retry Return it sends
  after a composer newline is what submits the text, instead of always
  `unverified`. A delivered input was recorded as uncertain by Butler's input
  queue, which then held back every later entry for that session.

- Cluster: the node identity (`cluster/identity.key`) is now encrypted at rest
  with XChaCha20-Poly1305 under a random per-node key in `keys/node.key`, kept
  owner-only and apart from the cluster state, so a copied state directory
  without that key reveals no secret (#746). The daemon reads it with no
  prompt and no OS keystore, the same on macOS, Linux and Windows. A plaintext
  identity from an older remuda is re-encrypted once at daemon start (same
  fingerprint) and the daemon log reports each file. Keep `keys/` out of
  backups. Downgrading after the migration fails safely: an older remuda
  reports the identity as invalid and does not replace it. See
  docs/credential-encryption.md.

- Cluster: a cluster-wide virtual file store (#742). `remuda vfs put|get|ls|
  stat|rm|resolve|sync|config` stores files at stable `remuda://CLUSTER/PATH`
  URIs (`remuda:///PATH` means this cluster). Content is sha256-addressed and
  kept on `--replicas N` nodes (default 3), chosen by rendezvous hashing over
  admitted nodes. Names replicate to every node, last writer wins. A node that
  has failed requests for longer than 120 s is treated as down, and the
  next-ranked node pulls a copy, so the copy count recovers after a node is
  lost. Daemon Lua gets read-only `remuda.vfs.stat/read/list`, which answer
  from local data only. Transfer uses four new requests on the existing
  authenticated cluster channel, served by the listener with their own
  per-peer rate limit and refused on the remote-control front. Any admitted
  node may read and write every path. Files are limited to 64 MiB. Existing
  per-node paths are unchanged. Design: docs/design-742-cluster-vfs.md.

- Lua: `remuda.session.new` and `remuda.new` now return `name, instance_id`, the
  ID of the launch that call started, taken from the same registry insertion
  (never a later lookup by name). Single-value assignments still get the name.
  Behavior change: `return new(...)` now passes both values, so `run_script`
  over MCP returns `name<TAB>instance_id`; write `return (new(...))` to keep the
  name alone. The ID identifies a launch; it is not a secret or proof of
  liveness. The daemon socket gains an additive request for this
  (same-user, mode 0600, same limits as `New`; remuda-butler issue #471).

- Shutdown: `remuda stop` now refuses when the daemon cannot verify who is
  asking (unreadable process ancestry, no peer PID, or a peer lookup error),
  as it already did for a caller inside one of its sessions (issue #639). To
  stop the intended daemon anyway, use the same server and runtime selection
  with `remuda -s NAME stop -f --yes --i-am-inside`. A caller whose parent has
  already exited still reads as outside and is allowed. The guard is advisory,
  not authentication: same-user access is not an authorization boundary and it
  makes no isolation claim.

- Windows: a repeated parent PID whose current process is newer than its
  child (a reused PID) no longer causes a false cycle refusal; the parent chain
  ends there as outside (#665). The incident mechanism is inferred, not
  reproduced. The check stays advisory, same-user, with no isolation claim.

- Windows: `remuda.caller().kind` is `outside` when the caller is in no
  session's job and its parent chain ends at a missing or reused PID. An
  unreadable or incomplete chain stays `unknown`; `outside` means only that
  the caller was not started inside a live session's tree. A process started
  in the instant before job assignment (issue #614) is not in the job, so
  `outside` is not an authorization or isolation claim.

- Windows: everything the session's program starts after it is put in its job
  ends when the session is reaped or closed, or when the daemon stops. A
  session that cannot be put in a job is not started. The one gap is the
  instant between program creation and job assignment; issue #614 tracks
  closing it with atomic job-list process creation (`CreateProcessW` with
  `PROC_THREAD_ATTRIBUTE_JOB_LIST`). A process that must outlive the session has to start outside
  remuda. While listed, processes in the job are `session` callers for
  `remuda.caller()`, including ones whose parent has exited.

- Windows: a session whose program is a `.cmd` or `.bat` file (for example an
  npm-installed agent) now refuses an argument that cmd.exe would act on
  instead of passing it as text: a line break, NUL, `%`, or `& | < > ^ ( )` outside
  double quotes. Before, such an argument could be cut, expanded, or run as a
  second command. The refusal is one line that names the argument's position
  and ends with `Next: start the .exe, or pass this text in a file.` Other
  arguments, and `.exe` programs, are unchanged. Limits: the guarantee ends at
  the batch file's own lines and holds for a file that forwards its arguments
  intact as `%*`, as npm shims do; rebuilding single parameters (`%~1`, `%~2`,
  `shift`) is not covered; the machine's cmd.exe settings still
  apply (AutoRun commands, and delayed expansion, where an argument with `!`
  may arrive changed).
- The storage file backend opens files through no-follow directory handles, refuses symlinked namespace and file components, and writes directories 0700 and files 0600 on Unix.
- Storage listings cap results at 1,024 valid names and eight path parts, filter names through API validation, and reject case-folded directory-part collisions.
- `remuda.storage.set_default("xdg")` selects a file backend for config, data, state and cache blobs under `REMUDA_STORAGE_ROOT` or the platform storage directories; secrets stay unavailable there.
- Windows file storage creates each resolver-based kind root with a protected owner, SYSTEM and Administrators DACL (including `%LOCALAPPDATA%\remuda\storage` for data) and verifies it once at selection. The verified handle is dropped and later I/O is path-based; `create_dir_all` ancestors are not verified, and `REMUDA_STORAGE_ROOT` selects the parent. File secrets stay unavailable until reparse-safe file handles land (slice D).
- Memory storage rejects unsafe names, case-fold collisions and writes over 1 MiB or 1024 entries per namespace and kind.
- `remuda send` now exits 75 when session input is busy and 74 when the PTY
  write times out (was 1); both messages include a `Next:` line (see #269).
- Windows now resolves `init.lua` and REPL history through the Config and State
  directories instead of `HOME`. Git Bash users relying on existing
  `$HOME/.config/remuda/init.lua` or
  `$HOME/.local/state/remuda/repl-history` files must move them to
  `%LOCALAPPDATA%\remuda\config\init.lua` and
  `%LOCALAPPDATA%\remuda\state\repl-history`, or set absolute XDG paths; the
  old files are not migrated.
- Fixed remote input batches refused after a late-submit abandonment: callers
  now get a retryable busy response because the bytes were not written (see
  #428).
- Lua: `remuda.storage.dir(kind)` returns an absolute per-user directory for
  `config`, `data`, `state` or `cache`. Absolute XDG values win on every OS;
  Windows falls back to Local AppData, and Linux/macOS use the XDG home layout.
  On Unix, data-home resolution no longer falls back to `USERPROFILE`.
  Relative XDG values are ignored; earlier data-home resolution accepted
  relative `XDG_DATA_HOME` values.
- Fixed secret prompt labels wrapping into rows that look like daemon tags by
  clipping them to terminal display width; see #363.
- Fixed: macOS credential store calls no longer show Keychain dialogs; locked or
  unavailable keychains return an `unavailable` reason instead. See #472.
- `install.sh`: when the install directory is not on `PATH` it now prints the
  command that fixes it, `Next: export PATH='<dir>':"$PATH"`, instead of only
  saying to add it to your shell profile. The directory is single-quoted, so
  the line is safe to paste whatever the directory is named (#395).
- `remuda cluster remote` now labels this node with the OS host name instead of
  reading the `HOSTNAME` environment variable; it uses `local` if the OS lookup
  fails.
- Lua: `remuda.fs.realpath(path)` resolves a path through every symlink, and
  `remuda.fs.is_symlink(path)` says whether the path itself is a link. Both work
  on Windows, where a junction counts as a link and a resolved path carries the
  verbatim prefix (`\\?\C:\...`). A mod no longer needs a shell `realpath` or
  `test -L` for this.
- Windows: `remuda.exe` now declares UTF-8 as its code page, so a path with
  non-ASCII characters (for example a Korean user folder) is the same string in
  Lua's `os.getenv`, `io.open` and `os.rename` as in the `remuda.*` words.
  Before, `os.getenv` returned bytes that `remuda.mkdir`, `remuda.fs.lock` and
  `remuda.process.run` refused ("invalid utf-8 sequence"). Needs Windows 10
  version 1903 or later; older systems behave as before. A non-ASCII path that
  a mod stored in a file under the old code page will not match after this
  change.
- `remuda _codex_tui` accepts `-c KEY=VALUE` after `--status PATH [--model M]`
  and forwards each one to the `codex app-server` it starts, so a mod can give a
  Codex session an MCP server; `remuda _codex_tui --help` prints the usage line.
- Nightly installs and upgrades now use immutable, versioned releases selected
  by `latest.json`; the newest ten are retained. Older installer copies still
  use the rolling `nightly` URL and may see a brief 404 while that compatibility
  release is replaced.
- Nightly builds are not published for ARM Linux (`aarch64-linux`) or Intel
  macOS (`x86_64-apple-darwin`); those platforms must build from source.
- Lua: `remuda.hostname()` returns the OS host name from the OS itself (not the
  environment), or `nil, error` if it is empty, not UTF-8, or holds a control,
  line-separator or bidi-control character. It is not sanitized for
  identifiers; callers slug it.
- Lua: `remuda.fs.lock(path)` takes an exclusive, non-blocking OS lock that is
  held until `handle:release()` or until the daemon exits, so a mod can tell
  that another live daemon already owns its home. When the lock is held it
  returns `nil, "held", info`; the info line is message text only.
- On Windows the data directory moved from `%USERPROFILE%\.local\share` to
  `%LOCALAPPDATA%`: the channel file (`%LOCALAPPDATA%\remuda\channel`), mods
  (`%LOCALAPPDATA%\remuda\mods`) and the update-check cache
  (`%LOCALAPPDATA%\remuda\cache`). `XDG_DATA_HOME` and `XDG_CACHE_HOME` still
  win. Nothing is migrated.
- The Windows installer now installs per user: `remuda.exe` goes to
  `%LOCALAPPDATA%\Programs\remuda\bin` and the channel file to
  `%LOCALAPPDATA%\remuda` (was `~\.local\bin` and `~\.local\share\remuda`).
  `REMUDA_INSTALL_DIR` and `XDG_DATA_HOME` still win.
- Lua: `remuda.process.run{...}` and `remuda.process{...}` take an optional
  `cwd`, an absolute path to an existing directory where the child starts.
  With `cwd`, the program must be an absolute path or a bare command name,
  and a bare name is searched on the absolute entries of PATH only.
