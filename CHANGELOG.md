# Changelog

## Unreleased

- The storage file backend opens files through no-follow directory handles, refuses symlinked namespace and file components, and writes directories 0700 and files 0600 on Unix.
- Storage listings cap results at 1,024 valid names and eight path parts, filter names through API validation, and reject case-folded directory-part collisions.
- `remuda.storage.set_default("xdg")` selects a file backend for config, data, state and cache blobs under `REMUDA_STORAGE_ROOT` or the platform storage directories; secrets stay unavailable there.
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
