# Changelog

## 2026-10-18

- New `remuda.daemon.socket_publication()` returns `{state = 'staged'}` or `{state = 'legacy', reason = ...}`, the same answer as the `SocketPublication` request, so mods such as `remuda butler doctor` can show it. A core without `remuda.daemon` does not have it; test the table first.
- The daemon now publishes its socket in two steps on macOS and Linux: it binds and chmods a private name, then renames it into place, so no client can connect before the daemon has recorded the node it will remove at shutdown. Platforms or paths that cannot do this (Android, BSD, a long runtime directory, a directory with an ACL) keep the old bind, and the reason is logged at start and reported by the new `SocketPublication` request. Set `REMUDA_SOCKET_PUBLICATION=legacy` in the daemon's environment to force the old bind. To roll back to an older `remuda`, stop the daemon first (`remuda stop`); an older binary does not know the staged leftovers, which are named `.r660-*` in the runtime directory and can be deleted while no daemon runs.

## 2026-10-02

- `remuda cluster init` warns when the cluster directory is missing but a saved fingerprint shows this machine had an identity before. The new identity does not restore peers; ask an admitted machine for a new invite and join again.

## 2026-10-01

- `reply:prompt_line` accepts an optional `preface`: text shown above the prompt on stderr, each line indented by two spaces and untagged, at most 32 lines of 256 characters (over a limit is an error). The label is unchanged: one line, cut at 256 characters. See `docs/deferred-reply.md`.
- A mod subcommand (for example `remuda butler doctor`) now loads its mod when it is not loaded yet, so it works as the first command after an install. The load is announced in one line, `remuda: started mod NAME`, because it runs the mod's `start`. A mod that cannot be loaded prints one line and a `Next:` line instead of a Lua traceback.

## 2026-09-29

- `--stdin` must come before the mod command; a trailing `--stdin` is a usage error; after `--` arguments pass to the mod literally.
- `process.run` closes the Unix process group when its leader exits, so output written afterward by a same-group background child is not returned.
- Runtime directories must be real, private, user-owned directories. A symlinked `REMUDA_RUNTIME_DIR` (including paths with a trailing slash or `/.`) is refused. On first start, `/tmp/remuda-$USER/remuda` is tightened to mode `0700`. Socket paths passed with `-s` that contain `..` are rejected. On macOS, `REMUDA_RUNTIME_DIR=/tmp` is refused because `/tmp` is a symlink; use `/private/tmp/<name>` or leave `REMUDA_RUNTIME_DIR` unset.
