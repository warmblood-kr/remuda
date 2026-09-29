# Changelog

## 2026-09-29

- `--stdin` must come before the mod command; a trailing `--stdin` is a usage error; after `--` arguments pass to the mod literally.
- `process.run` closes the Unix process group when its leader exits, so output written afterward by a same-group background child is not returned.
