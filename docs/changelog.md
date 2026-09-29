# Changelog

## 2026-09-29

- `--stdin` must come before the mod command; a trailing `--stdin` is a usage error; after `--` arguments pass to the mod literally.
- `process.run` closes the Unix process group when its leader exits, so output written afterward by a same-group background child is not returned.
- Runtime directories must be real, private, user-owned directories. A symlinked `REMUDA_RUNTIME_DIR` (including paths with a trailing slash or `/.`) is refused. On first start, `/tmp/remuda-$USER/remuda` is tightened to mode `0700`. Socket paths passed with `-s` that contain `..` are rejected. On macOS, `REMUDA_RUNTIME_DIR=/tmp` is refused because `/tmp` is a symlink; use `/private/tmp/<name>` or leave `REMUDA_RUNTIME_DIR` unset.
