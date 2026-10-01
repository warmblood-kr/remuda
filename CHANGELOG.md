# Changelog

## Unreleased

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
