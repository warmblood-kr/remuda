# Changelog

## 2026-10-06

- `remuda.process.exec(spec, on_done)` runs a bounded process without blocking its Lua image and delivers captured output to a Lua callback on the image event loop.

## 2026-10-02

- `remuda cluster init` warns when the cluster directory is missing but a saved fingerprint shows this machine had an identity before. The new identity does not restore peers; ask an admitted machine for a new invite and join again.

## 2026-10-01

- `reply:prompt_line` accepts an optional `preface`: text shown above the prompt on stderr, each line indented by two spaces and untagged, at most 32 lines of 256 characters (over a limit is an error). The label is unchanged: one line, cut at 256 characters. See `docs/deferred-reply.md`.
- A mod subcommand (for example `remuda butler doctor`) now loads its mod when it is not loaded yet, so it works as the first command after an install. The load is announced in one line, `remuda: started mod NAME`, because it runs the mod's `start`. A mod that cannot be loaded prints one line and a `Next:` line instead of a Lua traceback.

## 2026-09-29

- `--stdin` must come before the mod command; a trailing `--stdin` is a usage error; after `--` arguments pass to the mod literally.
- `process.run` closes the Unix process group when its leader exits, so output written afterward by a same-group background child is not returned.
- Runtime directories must be real, private, user-owned directories. A symlinked `REMUDA_RUNTIME_DIR` (including paths with a trailing slash or `/.`) is refused. On first start, `/tmp/remuda-$USER/remuda` is tightened to mode `0700`. Socket paths passed with `-s` that contain `..` are rejected. On macOS, `REMUDA_RUNTIME_DIR=/tmp` is refused because `/tmp` is a symlink; use `/private/tmp/<name>` or leave `REMUDA_RUNTIME_DIR` unset.
