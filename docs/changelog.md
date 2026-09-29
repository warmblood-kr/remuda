# Changelog

## 2026-09-29

- `process.run` closes the Unix process group when its leader exits, so output written afterward by a same-group background child is not returned.
