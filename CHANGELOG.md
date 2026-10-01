# Changelog

## Unreleased

- Nightly installs and upgrades now use immutable, versioned releases selected
  by `latest.json`; the newest ten are retained. Older installer copies still
  use the rolling `nightly` URL and may see a brief 404 while that compatibility
  release is replaced.
- Nightly builds are not published for ARM Linux (`aarch64-linux`) or Intel
  macOS (`x86_64-apple-darwin`); those platforms must build from source.
- Lua: `remuda.hostname()` returns the OS host name from the OS itself (not the
  environment), or `nil, error` if it is empty, not UTF-8, or holds a control
  character. It is not sanitized for identifiers; callers slug it.
