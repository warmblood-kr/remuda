# 069 — `remuda upgrade` reloads the daemon

## Before

`remuda reload` (step 068) could move a running daemon onto a new build,
and its host-backed sessions kept running. Nothing pointed users to it:
- `remuda upgrade` still said the daemon keeps the old version until
  `remuda stop`, which ends every session.
- The notice for a daemon of another build said "`remuda stop` replaces it".
- `reload` was missing from `remuda --help`.

## Desired outcome

- On Unix, after a successful install, `remuda upgrade` reloads the selected
  daemon into the binary it just installed (`reload::upgrade::after_upgrade`):
  - no daemon: nothing to move;
  - a daemon already on that build: left alone;
  - otherwise, `Request::Reload`, then the post-check (`daemon now BUILD`).
- Only a reload that was rolled back fails `upgrade` (exit 1). Any other
  refusal changed nothing, so upgrade exits 0 and says why the daemon kept its
  build. Examples: an in-process session blocks it, or an older daemon or
  older installed build cannot reload.
- `--no-reload` installs only. Windows keeps the old wording.
- The skew notice on Unix leads with `remuda reload`; `remuda stop` stays
  for a version check that failed partway, and for Windows.
- `reload [--check]` is in both help texts and the README.

## Expected

- `native/tests/reload_upgrade.rs`, with a wrapper script as the installed
  build. The wrapper names another version and runs this binary, so a real
  reload happens:
  - no daemon → `NoDaemon`;
  - same build → `Current`, and the daemon pid is unchanged;
  - another build → `Reloaded`, a new pid, and the hosted session answers;
  - an in-process session → `Kept("blocked: …")`, and it still answers;
  - an installed build that cannot be a candidate → `Kept`;
  - a candidate that dies before serving → `Failed("… rolled back …")`.
- `n_plus_one_upgrade_*`: across N and N+1 in both directions, the session
  survives, and the outcome is a reload or a reasoned `Kept`.
- A unit test: the notice's cure survives an 80-column crop.

## Actual

```
$ cargo test -p remuda-native --test reload_upgrade
test result: ok. 7 passed; 0 failed

# N/N+1 against a pre-reload build, #789's, #792's and #793's
upgrade: Kept("the running daemon predates `remuda reload`, …")
upgrade: Kept("reload without --check is not implemented yet; …")
upgrade: Reloaded("reloaded: 0.1.0+git.dd784c8dd319 now serves this socket …")
test result: ok. 8 passed; 0 failed

# red: without the same-build check
panicked at native/tests/reload_upgrade.rs:157:5   (pid changed)
# red: a blocked report taken as reloaded
panicked at native/tests/reload_upgrade.rs:195:9

$ remuda -s g ls      # daemon on another build
remuda: the daemon is not this build — `remuda reload` moves it onto this one and keeps host-backed sessions, or says why it cannot. It is …
```

Limits:
- `upgrade` moves only the selected server's daemon (`-s NAME`).
- `upgrade` reloads into this binary's own path. An installer that writes
  elsewhere leaves that path's build, which reads as `Current`.
