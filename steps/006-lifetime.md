# 006 — Ending a session, and not stealing another daemon's name

## Before

정수님, 2026-09-10, on being shown that `Request` can create a session and never
end one:

> 세션 닫는 것 바로 넣어야겠네요. 세션 안에서 스스로 터미널을 닫는 경우도 세션
> 정리가 되어야겠고요.

and, in the same minutes:

> remuda 데몬은 한 노드에서 여러개 띄울 수도 있겠습니다. 마치 claude code를
> home dir 바꿔서 여러 설정을 띄울 수 있듯이.

Three things, and they are one step because all three are about **how long a
thing lives** — a session, and the daemon holding it. Building the image (007)
on top of these would make the worst of them worse: a daemon that gets silently
displaced today loses sessions; once it holds a live Lua image, it loses that too.

### What is already true, measured before designing

Self-exit is **already detected**. This is the correction to make first, because
it changes what "정리" has to mean:

```
$ remuda ls          probe  80x24  live  idle 484s
$ remuda send probe "exit"
$ remuda ls          probe  80x24  dead  idle 2s        ← observed, not assumed
$ remuda capture probe                                  ← still readable
                     exit
$ remuda send probe "echo still-there"
                     remuda: agent process has exited   exit=1
```

So death is observed, writes are refused with a sentence a person can read, and
the last screen survives. What is missing is only **collection**: a dead session
stays in the registry forever, and `ls` grows without bound.

And `core` already has both halves — with **zero production callers**:

```
core/src/registry.rs:93   pub fn remove(&self, name: &str) -> Option<Arc<Session>>
core/src/registry.rs:98   pub fn reap(&self) -> Vec<String>
callers                   core/tests/invariants.rs only
```

This is the third time today the tree turned out to hold the capability and not
reach it. The work is a `Request` variant and a CLI verb, not a mechanism.

### The decision inside "정리": death marks, it does not delete

Reaping the moment a child exits is the obvious reading, and it is wrong here.
The screen of a session that just died **is the evidence for why it died**, and
this is a tool for holding agents — that screen is the first thing anyone wants.
Auto-collection destroys it silently, at exactly the moment it became valuable.

tmux split on the same question and made it a setting (`remain-on-exit`,
defaulting to collect). Our default should be the other one: we hold agents, not
shell windows.

⇒ **Death marks. Deletion is always explicit.** One verb covers both states:

```
close on a live session   terminate the child, then drop the entry
close on a dead session   drop the entry (the screen goes with it)
```

### The daemon defect, correctly diagnosed this time

I first reported this as "auto-start never reaps the old daemon." That was wrong,
and the real cause is worse. `daemon.rs:49`:

```rust
// A socket left by a crashed daemon would make bind fail forever. Removing
// it is safe only because a *live* daemon is detected by connecting, which
// is what the client does before it ever starts one.
let _ = std::fs::remove_file(path);
let listener = UnixListener::bind(path)?;
```

The new daemon **unlinks a live peer's socket** and binds its own. The peer does
not die — it keeps running, unreachable, holding its pty children. Four were live
on this machine when measured. The comment's safety argument holds only on the
*client* path (connect, and start one only on failure); a human typing
`remuda daemon`, or a rebuilt binary, walks straight past it.

정수님's second message is half the fix. If several daemons per node are normal,
then the problem was never "two are running" — it is **"two wanted the same name,
and the later one displaced the earlier one in silence."** Named daemons live side
by side; same-named ones must refuse, not displace. Silent displacement is the
worst of the three options and it is what we do now.

The plumbing for names already exists and only the CLI is closed:

```rust
daemon.rs:29   pub fn socket_path(server: &str) -> PathBuf {
                   let base = env REMUDA_RUNTIME_DIR else XDG_RUNTIME_DIR else /tmp/remuda-$USER
                   base.join("remuda").join(format!("{server}.sock"))
bin/remuda.rs:31   let path = daemon::socket_path("default");   ← hardcoded; this is the whole gap
```

`REMUDA_RUNTIME_DIR` is the "different home dir" axis he named; `socket_path`'s
argument is the "several under one runtime" axis. Both are live.

## Desired outcome

**`remuda close <name>`**, and `remuda.close(name)` in a script. It works on a
live session (terminate, then forget) and on a dead one (forget). Closing a name
that does not exist is an error with a sentence, not a silent success.

**Terminating means the child actually goes.** `AgentProcess` grows one method;
`PtyAgent` implements it with the `Child::kill` it already holds. A session whose
entry we dropped must not leave a process behind — that is the bug we are fixing,
not a smaller version of it.

⚠ **A close must not race an attach.** `Session` invariant 3 refuses writes while
a human is attached; close is not a write, but tearing the pty out from under an
attached terminal is worse than refusing. Close on an attached session should be
refused with the same sentence-shaped error, and the human detaches first.

**`ls` stops growing.** Dead entries persist until closed — that is the point —
but there is now a way to end them, so unbounded growth becomes a choice rather
than the only outcome.

**`remuda -s <name>`**, defaulting to `default`, so several daemons coexist on one
node. `REMUDA_RUNTIME_DIR` keeps working as the other axis and needs no flag.

**A daemon never unlinks a live peer's socket.** Startup must distinguish a stale
socket from a live one — connect first, and only unlink when nothing answers.
A same-named daemon started against a live one exits with a sentence saying which
name is taken.

### Deliberately out of scope

- Reaping policy (max dead sessions, TTL). YAGNI until `ls` actually hurts;
  the verb is what was missing, not a policy.
- Killing a *daemon* from the CLI (`remuda kill-server`). Different lifetime,
  different step, and the socket-collision fix removes the reason we wanted it.
- Signals other than the default terminate (`close --signal`). One verb first.

## Expected

Written before building:

1. `remove` and `reap` need no change — the step is protocol plus a `terminate`.
2. `terminate` on `AgentProcess` will want a default implementation for the
   scripted double, and I expect the honest default to be "no process, nothing to
   do" — but I also expect that to leave the double's `is_alive` still true,
   which would make a test lie. If so the default is wrong and each backend
   states its own.
3. Dropping the last `Arc<Session>` already closes the pty master, which SIGHUPs
   the child. So `remove` alone may *appear* to work. I expect it to be racy
   (attach holds a handle) and to leave the child alive whenever anyone else does
   — which is precisely why the explicit terminate exists rather than relying on
   drop.
4. The socket-collision fix is `UnixStream::connect(path).is_ok()` before the
   unlink — the client already does exactly this, so the daemon is duplicating a
   check it could have shared.
5. `v1.lua` passes unchanged: `close` is an addition, and nothing frozen changes.
6. I expect closing an attached session to be the case I get wrong first, because
   it is the only one where the refusal is a design choice rather than an error.

## Actual

Expected #2 was wrong, and the way it was wrong is the most important thing
this step found — so it leads, ahead of the confirmations.

**Expected #2/#3 assumed idempotence that was not there — measured against the
real backend, not reasoned about it.** The first `PtyAgent::terminate` called
`self.child.kill()` unconditionally, on the theory (documented in the code
comment at the time) that "a signal to an already-exited pid still returns
success." That is true of a *zombie* — a pid not yet reaped — but `is_alive`
calls `try_wait`, which on unix reaps the child the instant it returns `Some`.
After that the pid is gone outright, not a zombie, and signalling it fails:

```
$ remuda new p2 -- bash; sleep 1
$ remuda send p2 "exit"; sleep 1
$ remuda ls
p2                     80x24   dead  idle 1s
$ remuda close p2
remuda: agent io error: No such process (os error 3)
exit=1
```

This is exactly the scenario 정수님 named directly — "세션 안에서 스스로 터미널을
닫는 경우도 세션 정리가 되어야겠고요" — and the first implementation failed it.
Fixed by checking `is_alive` before signalling, in `PtyAgent::terminate` itself
rather than pushing the check up to every caller:

```rust
fn terminate(&mut self) -> Result<()> {
    if !self.is_alive() {
        return Ok(());
    }
    self.child.kill().map_err(io)
}
```

Re-measured after the fix — the same sequence, now succeeding and actually
removing the entry:

```
$ remuda new p2 -- bash; sleep 1
$ remuda send p2 "exit"; sleep 1
$ remuda ls
p2                     80x24   dead  idle 1s
$ remuda close p2
exit=0
$ remuda ls
no sessions
```

Pinned as unit tests against the scripted double, so this cannot regress
silently (`core/tests/invariants.rs`, new suite "step 006"):

```
$ cargo test --release -q --test invariants -- --list | grep close_
close_is_refused_while_attached_and_the_session_survives: test
close_on_an_already_dead_session_is_not_an_error: test
close_on_an_unknown_name_is_none_not_an_error: test
close_terminates_a_live_session_and_stops_tracking_it: test

$ cargo test --release -q --test invariants
running 21 tests
.....................
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Expected #6 confirmed — the socket-collision fix refuses instead of
displacing:

```
$ remuda new probe -- bash; sleep 1
$ remuda daemon              # started directly, socket already bound
remuda: daemon: a daemon is already listening at /run/user/1001/remuda/default.sock
— pick a different name (remuda -s <name>) or stop it first
exit=1
$ ps -eo cmd | grep '[r]emuda daemon'
target/release/remuda -s default daemon      # exactly one — the original survives
```

`-s` gives two daemons, each with its own session list, over the same runtime
directory — and surfaced a second bug of the same shape while proving it:

```
$ remuda -s alt new named -- bash; sleep 1
$ remuda -s alt ls
named                  80x24   live  idle 1s
$ remuda ls                       # default, unaffected
no sessions
$ ls $XDG_RUNTIME_DIR/remuda/
alt.sock  default.sock
```

The first cut of `-s` passed this manually only because a daemon happened to
already be running on `default`. A clean-state run exposed that `start_daemon`
re-spawned itself as bare `remuda daemon` — dropping `-s alt` — so the child
came up on `default.sock` while the parent waited on `alt.sock` and timed out.
Fixed by threading `server` through `with_daemon`/`start_daemon` and passing
`-s <server>` to the spawned child. Confirmed after the fix:

```
$ ps -eo pid,ppid,cmd | grep -i remuda
1889735  948  target/release/remuda -s alt daemon
1890043  948  target/release/remuda -s default daemon
```

Refusal while attached (Expected #2, unit-tested rather than via a shelled-out
attach — a real terminal attach in a non-interactive harness is exactly the
kind of flaky the governance store warns against; the `Session`-level
invariant is what actually matters and the scripted double exercises it
directly):

```rust
let held = session.attach().expect("attach");
assert!(matches!(registry.close("worker"), Some(Err(AgentError::Attached))));
assert!(alive.load(Ordering::SeqCst), "a refused close must not have touched the process");
assert!(registry.get("worker").is_some(), "and the entry survives");
drop(held);
assert!(matches!(registry.close("worker"), Some(Ok(()))));
```

Expected #1, #4 confirmed by inspection and by the diff being exactly as
small as predicted: `Registry::remove`/`reap` needed no changes; `close` is
`terminate` (refuses while attached) then `remove`, ~10 lines in
`core/src/registry.rs`.

Expected #5 — full suite, `v1.lua` included, unchanged:

```
$ cargo test --release -q
... (11 test binaries)
test result: ok. 21 passed  (invariants)
test result: ok. 10 passed
test result: ok. 1 passed
test result: ok. 4 passed
test result: ok. 8 passed
test result: ok. 4 passed
..v1 ok
test result: ok. 4 passed      ← v1.lua, unchanged
$ cargo clippy --release --all-targets -q   (clean)
$ cargo fmt --check                          (clean)
$ python3 scripts/check-steps.py
ok — 7 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py
ok — 10 principles, every named mechanism exists (7 CI jobs, 11 denied paths, 98 test fns seen)
```

Lua binding, confirmed from a script rather than only the CLI — including that
a second `close` on an already-removed name raises, matching every other
refusal in this vocabulary (`value()` turns `Response::Error` into a raised
error, never a return value a script could forget to check):

```
$ cat probe.lua
remuda.new("scripted", {"bash"})
remuda.close("scripted")
local ok, err = pcall(remuda.close, "scripted")
print("second close raised:", not ok, err)
$ remuda run probe.lua
second close raised:	true	runtime error: no such session: scripted
$ remuda ls
no sessions
```

Not measured: fd exhaustion from many dead-but-unclosed sessions (flagged as a
risk in Before, not reproduced — would need thousands of sessions to observe
against the OS limit, out of proportion to this step).
