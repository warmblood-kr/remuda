# 009 — versioning, channels, and a one-line install

정수님, 2026-09-10: *"versioning system을 수립합시다. staging이나 nightly build 같은
것도. one line install 수단을 마련합시다. … 업그레이드도 remuda upgrade 처럼 간편하게
되도록. 업데이트 알림도 뜨도록. 그러려면 현재 최신버전이 몇인지도 게시해둬야겠죠."*
And on the shape: *"뭔가 circle/dist channel 같은걸 둬도 좋겠습니다. … rustup 등 방식
참고해도 좋겠네요. 컨벤셔널하게."*

## Before

The repo went public today with `version = "0.0.0"` and no way to get the binary
except `git clone && cargo build`. Concretely, what did not exist:

```
version string     0.0.0, workspace-wide, never printed by the binary
                   `remuda --version` was not a command — it fell through to USAGE
distribution       none. no release, no asset, no tag
install            clone the repo and build Lua from source
upgrade            git pull && cargo build
"am I current?"    unanswerable — nothing published what current is
```

Nothing here is a defect in the running code; it is that the code has no way to
reach anyone. And the one axis that could not be assumed: **`native/` depends on
`nix` and speaks over a unix socket**, so whether a Windows one-liner is even
honest was an open question, not a packaging detail.

## Desired outcome

Two channels in the shape rustup uses — `stable` from a `vX.Y.Z` tag, `nightly`
rolling on every push to `main` — a published index saying what each one is now,
a copy-paste one-liner per platform that genuinely works, `remuda upgrade`, and
a once-a-day update notice that never makes a command slower.

And, on Windows: a **measured** answer. Either it is small cfg work and gets
done, or it is real porting and the README says "not yet". A README promising an
install that leaves a user with no binary is worse than one that admits the gap.

### The two laziness decisions, stated so they can be argued with

- **`remuda upgrade` re-runs the install script** (`curl` → a temp file → `sh`)
  rather than adding an HTTP/TLS client to the binary. Install and upgrade
  become one code path, and the dependency list does not grow a networking stack
  for a command run a handful of times in a binary's life.
- **The update check is a detached child, never an await.** The current run
  reads a cache file; a stale cache spawns a background `curl` whose result the
  *next* run reads. The trade is explicit: a notice may arrive one run late, and
  in exchange no command can ever be slowed or hung by the network.

## Expected

Written before running, so they can be contradicted:

1. **Windows will not compile**, and the errors will be `nix` and unix sockets —
   not a small cfg matter. If so, ship Linux+macOS and say so.
2. `REMUDA_VERSION` injected at build time will reach `--version`; without it,
   `CARGO_PKG_VERSION` will.
3. The version ordering will need care: a nightly of `0.1.0` must rank *below*
   `0.1.0`, or every nightly user is told to "upgrade" to the release they are
   ahead of.
4. **The install script will work first try.** It is 90 lines of `curl`, `tar`
   and `sha256sum`, all of it well-trodden.
5. Replacing the binary by rename will leave a running daemon untouched.

## Actual

### 1 — Windows: confirmed blocked, and it is porting, not gating

The first probe was uninformative — `x86_64-pc-windows-msvc` from Linux dies in
the vendored Lua C build, which says nothing about *our* code:

```
warning: mlua-sys@0.12.0: /home/toracle/.cargo/registry/…/lua-5.4.9/loadlib.c:150:10:
         fatal error: windows.h: No such file or directory
error: failed to run custom build command for `mlua-sys v0.12.0`
```

Re-run against `x86_64-pc-windows-gnu` with the mingw toolchain, which gets past
the C build and reaches `remuda-native`. **10 errors, and every one is a
platform facility with no Windows equivalent in this code:**

```
error[E0433]: cannot find `sys` in `nix`
   --> native/src/client.rs:115:18
    |
115 |         use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
    |                  ^^^ could not find `sys` in `nix`

error[E0433]: cannot find `unix` in `os`
  --> native/src/daemon.rs:20:14
   |
20 | use std::os::unix::net::{UnixListener, UnixStream};
   |              ^^^^ could not find `unix` in `os`
   |
note: found an item that was configured out
  --> /rustc/…/library\std\src\os\mod.rs:29:4
   |
   = note: the item is gated here

error[E0432]: unresolved import `std::os::fd`
  --> native/src/lib.rs:28:18
   |
28 |     use std::os::fd::AsRawFd;
   |                  ^^ could not find `fd` in `os`

error[E0433]: cannot find `libc` in `nix`
  --> native/src/lib.rs:31:14
   |
31 |         nix::libc::ioctl(
   |              ^^^^ could not find `libc` in `nix`

error[E0599]: no method named `as_raw_fd` found for struct `Stdout` in the current scope
  --> native/src/lib.rs:32:31
   |
32 |             std::io::stdout().as_raw_fd(),
   |                               ^^^^^^^^^
   |
help: there is a method `as_raw_handle` with a similar name

error: could not compile `remuda-native` (lib) due to 10 previous errors
```

Grouped, that is three jobs, none of them a `cfg` attribute:

```
IPC transport      the daemon IS a unix socket (bind/connect/accept)   → named pipes
terminal control   termios raw mode via nix, for `attach`              → console modes
window size        TIOCGWINSZ ioctl on a raw fd                        → GetConsoleScreenBufferInfo
```

⇒ **Expectation 1 held.** Windows is stopped on this axis. Linux and macOS ship;
the README says "not yet" and there is no `install.ps1`.

### 2 & 3 — version string and ordering: as expected

```
$ ./target/debug/remuda --version
remuda 0.1.0
$ REMUDA_VERSION=0.1.0-nightly.20260910.d1645e7 cargo build -q -p remuda-native --bin remuda
$ ./target/debug/remuda --version
remuda 0.1.0-nightly.20260910.d1645e7
```

The ordering rule expectation 3 warned about is a unit test
(`a_release_outranks_its_own_nightlies_and_the_triple_wins_first`), and it is
also visible live — this binary is a *nightly* of 0.1.0, so an index publishing
stable `0.1.0` correctly tells it to upgrade:

```
$ XDG_CACHE_HOME=/tmp/rq/cache XDG_DATA_HOME=/tmp/rq/data ./target/debug/remuda --version
remuda: 0.9.9 is out on the stable channel (you have 0.1.0-nightly.20260910.d1645e7) — `remuda upgrade`, or REMUDA_NO_UPDATE_CHECK=1 to silence this
remuda 0.1.0-nightly.20260910.d1645e7
```

The notice goes to **stderr**, so `remuda capture | …` is unaffected, and it is
suppressed on `daemon` (whose stderr is the client's only diagnostic when
start-up fails) and on `mcp` (whose streams a client owns).

**The "never blocks" claim, measured on the worst case** — no cache at all, and
an index URL that genuinely does not resolve yet:

```
$ rm -rf /tmp/rq/cache
$ time env XDG_CACHE_HOME=/tmp/rq/cache ./target/debug/remuda --version
remuda 0.1.0-nightly.20260910.d1645e7
env … --version  0.00s user 0.00s system 74% cpu 0.003 total

$ sleep 3; ls -la /tmp/rq/cache/remuda/
-rw-r--r-- 1 toracle toracle    0 Sep 10 13:17 update-check.json

$ time env XDG_CACHE_HOME=/tmp/rq/cache ./target/debug/remuda --version
env … --version  0.00s user 0.00s system 55% cpu 0.004 total
```

3ms, and this was not a lucky path: the fetch really failed (Pages is not live),
the trailing `touch` stamped a zero-byte cache, the JSON parse failed silently,
and the second run did **not** respawn a curl. That stamp is the whole reason an
offline machine does not spawn a `curl` per command.

### 4 — expectation 4 was WRONG, twice, and this is the useful part

Both defects were found by serving a real release over http and running the real
`docs/install.sh` against it, with only the two hostnames edited:

```
### 3. serve both trees
edits made to install.sh:
17c17
< INDEX=https://warmblood-kr.github.io/remuda/latest.json
---
> INDEX=http://127.0.0.1:8731/pages/latest.json
76c76
< base="https://github.com/$REPO/releases/download/$tag"
---
> base="http://127.0.0.1:8731/releases/download/$tag"

### 4. run the installer as a new user would
install.sh: fetching remuda 0.1.0 (stable, x86_64-unknown-linux-gnu)
install.sh: remuda-0.1.0-x86_64-unknown-linux-gnu.tar.gz is not listed in SHA256SUMS
```

**Defect one.** `sha256sum ./*.tar.gz` writes the name with a `./` prefix; the
installer grepped for `" $asset$"` and matched nothing. The fix is an exact
field comparison in `awk` rather than a pattern match — the asset name is full
of dots, so a regex over it was the wrong tool from the start — plus the
workflow writing plain names so a human's `sha256sum -c SHA256SUMS` works too.

**Defect two**, found in the same session by running `remuda upgrade` for real:

```
### b. remuda upgrade composes
remuda: upgrading on the nightly channel…
curl: (22) The requested URL returned error: 404
channel file now: stable            ← and remuda reported SUCCESS
```

`curl … | sh` reports the *pipeline's last* status. A 404 fed an empty script
into a shell that exited 0, so a failed upgrade was indistinguishable from a
finished one. Fixed by landing the script on disk under `set -e` first — which
also means a truncated download is never executed. After:

```
remuda: upgrading on the nightly channel…
curl: (22) The requested URL returned error: 404
remuda: the installer exited with exit status: 22
channel file now: stable
```

⇒ Both are in PRINCIPLES.md §11 and §12. Neither was reachable by reading the
code; both were obvious within seconds of a real fetch.

**The full rehearsal, after the fixes** — both channels, and the branch that
matters most:

```
### 1. package exactly as .github/workflows/release.yml does
d4c9fdad9259586fadb50e0d069c3200b28e8b16f0a00f901f45138ef6f25b99  remuda-0.1.0-x86_64-unknown-linux-gnu.tar.gz
remuda

### 4. run the installer as a new user would
install.sh: fetching remuda 0.1.0 (stable, x86_64-unknown-linux-gnu)
install.sh: remuda 0.1.0 -> /tmp/remuda-lab/home/.local/bin/remuda (stable channel)
install.sh: /tmp/remuda-lab/home/.local/bin is not on your PATH — add it to your shell profile

### 5. what landed
-rwxr-xr-x 1 toracle toracle 2083496 Sep 10 13:26 /tmp/remuda-lab/home/.local/bin/remuda
channel file: stable
remuda 0.1.0

### 6. nightly channel picks the nightly version and the rolling tag
install.sh: fetching remuda 0.1.0-nightly.20260910.d1645e7 (nightly, x86_64-unknown-linux-gnu)
install.sh: remuda 0.1.0-nightly.20260910.d1645e7 -> /tmp/remuda-lab/home/.local/bin/remuda (nightly channel)
channel file now: nightly

### 7. THE SECURITY BRANCH — a corrupted tarball must be refused
install.sh: fetching remuda 0.1.0 (stable, x86_64-unknown-linux-gnu)
sha256sum: WARNING: 1 computed checksum did NOT match
install.sh: checksum mismatch on remuda-0.1.0-x86_64-unknown-linux-gnu.tar.gz — refusing to install
-> refused, as it must
```

Step 6 uses the `./`-prefixed `SHA256SUMS` form and step 1 the plain form, on
purpose: the normalization is exercised both ways.

### 5 — upgrading under a running daemon: held

The claim is that landing the binary by rename leaves a running process alone.
Started a session, reinstalled underneath it, then asked the *live* session a
question whose answer cannot appear in the echo of the question (§4):

```
### a. start a session, then reinstall underneath the running daemon
probe                  80x24   live  idle 1s
daemon pid 183785, binary inode 11570976

--- reinstalling (this is what remuda upgrade runs) ---
install.sh: remuda 0.1.0 -> /tmp/remuda-lab/home/.local/bin/remuda (stable channel)
binary inode now 11570978 (changed: yes)

--- the daemon and its session must have survived ---
daemon 183785 still alive
42-landed
-> the running session answered after its binary was replaced
```

The inode changed, so the replacement really happened; the daemon kept its old
one and its pty. A `cp` over the running file would have been `ETXTBSY`.

That daemon — pid 183785 — is the same process across every rehearsal in this
document. It has now been reinstalled out from under several times and is still
serving the session it opened.

### The new gates, watched going red (§2)

`install-path-is-consistent` compares the release matrix against the installer's
platform list, because nothing else in the build compares a YAML matrix to a
shell script:

```
=== NEGATIVE CONTROL 1: a target install.sh offers but nothing builds ===
the installer and the release workflow disagree:

  - install.sh offers aarch64-unknown-linux-gnu, but release.yml does not build it — a 404 for those users

A platform in one file and not the other is a broken one-liner in the README — the only place it shows up is a user's terminal.
exit=1
```

The `sh -n` control took two attempts, and the first failure is worth keeping —
it is §3 (match the *reason*) landing on the control itself:

```
=== NEGATIVE CONTROL 2: sh -n on a syntax error ===
exit=0
```

The plant was `if [ -z "$x" ; then` — which is **not a syntax error**. `[ -z "$x"`
is a perfectly valid command invocation and the missing `]` fails at runtime, so
`sh -n` was right and the control was wrong. A real parse error:

```
=== which sh? ===
lrwxrwxrwx 1 root root 4 Jan  5  2023 /bin/sh -> dash
=== NEGATIVE CONTROL 2b: an unterminated if (a real parse error) ===
docs/install.sh: 112: Syntax error: end of file unexpected (expecting "fi")
exit=2
```

Both controls, extracted verbatim out of `ci.yml` and run:

```
extracted 2 control bodies verbatim from ci.yml
=== check-install.py rejects a platform ===
ok — 3 target(s) built and offered: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu
  -> control passed
=== sh -n rejects a broken installer ===
  -> control passed
```

### The whole suite

`clippy` went red on the way here, which is the gate doing its job: `main` grew
to 102 lines against a 100 budget once the notice and `upgrade` arms landed.
Split out as `announce_update` and `run_upgrade`, for the same reason
`list_sessions` was split out before. `check-comments.py` then caught the module
header at 22 lines against its cap of 20. Both are green below.

```
### cargo fmt --all --check
(silent = clean)

### cargo clippy --workspace --all-targets -- -D warnings
    Checking remuda-native v0.1.0 (…/remuda/native)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s

### cargo test --workspace --all-targets
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s

### the four checkers
ok — 12 principles, every named mechanism exists (9 CI jobs, 11 denied paths, 114 test fns seen)
ok — 9 step(s), each with before, desired, expected and captured actual
ok — 112 doc comment(s) within cap (item 3, module 20)
ok — 3 target(s) built and offered: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu

### sh -n docs/install.sh
(silent = clean)
```

55 tests, three of them new (version ordering, shell quoting, a non-empty
version string). The `wasm-boundary` gate is untouched and still passes.

## What this step did NOT do

Stated plainly, because the section above is otherwise easy to read as "the
release system works":

- **No release has ever run.** `release.yml` has never executed — it cannot,
  without merging to `main`. Every claim about *packaging* is proven (the
  tarball, the checksums, the install, the upgrade, all of it built and run
  here); every claim about the **workflow orchestrating** that — artifact
  upload/download, `gh release create`, the commit back to `docs/latest.json`,
  the `paths-ignore` guard that stops that commit starting another nightly — is
  read-and-reasoned, not observed.
- **macOS is unbuilt.** Both Darwin targets are in the matrix on the strength of
  the code being unix; no Apple machine was involved. The Linux target is the
  one actually compiled and installed.
- **GitHub Pages is not on.** `latest.json` and `install.sh` are committed under
  `docs/`, and serving them needs Pages pointed at `main` `/docs` — a repo
  setting, deliberately not touched. Until that is flipped, every published URL
  here 404s, which is exactly what the update check was measured against above.
- **`shellcheck` did not run.** It is not installed here and the release
  download 500s through this network, so `sh -n` is the only shell lint that has
  actually been watched work — and it is the only one wired into CI, rather than
  adding a gate nobody has seen go red.
- **No tag was pushed.** Cutting `v0.1.0` is a follow-up, not part of this step.
