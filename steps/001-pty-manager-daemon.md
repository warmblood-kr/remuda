# 001 — a pty manager with daemon mode, a session list, and attach

정수님, 2026-09-10: *"우선은 lower layer 부터 빌드해나갑시다. 일종의 tmux 같은 pty
manager, which support daemon mode. with session list so that user can select a
session to attach."*

## Before

`remuda-core` could hold sessions and `remuda-native` could put a real process on
a real pty, and 29 tests said so. But **none of it was reachable by a person.**
There was no binary, no daemon, and no way to look at a session:

- every session died with the process that created it, so nothing could outlive
  a test function — and a session is supposed to be where an agent gets logged
  in by a human and then works for hours;
- the only way to read a screen was `screen_text()` from inside Rust;
- `Session` deliberately exposes no raw write (invariant 1), so there was no way
  for a human to type into one at all.

That last point is not an oversight to patch — it is a real collision. A person
at a terminal types their own Enter, which is exactly the operation invariant 1
refuses to expose.

## Desired outcome

`remuda` becomes a command you can actually use, and attaching becomes possible
without weakening the invariant that made it hard:

- a daemon owns the sessions and outlives every client;
- `remuda ls` lists them so a user can pick one;
- `remuda attach <name>` hands the terminal over, `Ctrl-\` gives it back;
- `remuda capture <name>` reads a screen with no terminal at all, because a
  machine looking at a session should not have to lock a human out of it;
- the daemon starts itself, because 정수님 asked for this to be seamless and a
  tool that makes you start a server first is not;
- and raw keystrokes exist **only** while exactly one viewer holds the session,
  with orchestrated `send_line` refused — not queued — for the duration.

## Expected

1. `remuda ls` on a machine with no daemon starts one and prints `no sessions`.
2. Two sessions can be created, listed, and stay separate — an instruction sent
   to one never appears in the other.
3. A duplicate name and a missing session both fail **in words**, not silently.
4. The daemon is still alive after every client process has exited.
5. Attaching paints what is already on screen, forwards keystrokes, streams
   output back, and releases the session on `Ctrl-\`.
6. While attached, `send_line` returns an error **and leaves no bytes in the
   pty** — a refusal that still wrote would be worse than no refusal.
7. A `size` below the 80×24 floor, arriving as JSON, is clamped rather than
   honoured. A constructor that clamps is worth nothing if the wire goes round it.

## Actual

### The command, run end to end

```
$ remuda ls                    # 데몬 없음 → 자동 기동
no sessions

$ remuda new alpha
$ remuda new bravo -- sh -c "while :; do sleep 1; done"

$ remuda ls
alpha                  80x24   live  idle 0s
bravo                  80x24   live  idle 0s

$ remuda send alpha "echo $((6*7))-cli"   # 산술: 에코로는 42가 나올 수 없다
$ remuda capture alpha | tail -3
➜  pty-core-wip echo $((6*7))-cli
42-cli
➜  pty-core-wip git:(main) ✗

$ remuda send nosuch hi
remuda: no such session: nosuch
$ remuda new alpha
remuda: name taken: alpha

$ pgrep -a remuda   # 데몬은 클라이언트가 전부 끝난 뒤에도 살아 있다
2361813 /home/toracle/projects/pty-core-wip/target/debug/remuda daemon
```

Expectations 1–4 met. `42` cannot come from the pty echoing the command, so the
capture shows the shell ran rather than the terminal repeated.

### Attach, exercised through a real terminal

The shipped binary is run **inside a pty of this crate's own making**, which is
the only way to reach raw mode and the detach key — those paths do not exist
without a controlling terminal.

```
$ cargo test --workspace --all-targets
test sessions_are_listed_and_kept_apart ... ok
test a_taken_name_is_refused_in_words ... ok
test a_wire_size_below_the_floor_is_clamped_not_honoured ... ok
test a_human_attaches_through_a_real_terminal_and_detaches_with_ctrl_backslash ... ok

test result: ok. 16 passed  (core invariants)
test result: ok.  4 passed  (daemon, over a real socket)
test result: ok.  8 passed  (live pty sessions)
test result: ok.  1 passed  (system clock)
```

Expectations 5–7 met.

### Two defects the "expected vs actual" step caught — neither was foreseen

**1. The startup error named the wrong wall.** Auto-start discarded the daemon's
stderr, so a daemon that failed for a perfectly nameable reason produced a
timeout message instead:

```
before:  remuda: daemon did not come up at /tmp/…/default.sock
after:   remuda: daemon did not come up at /tmp/…/default.sock
                 — remuda: daemon: path must be shorter than SUN_LEN
```

The first message sends you looking at timing and permissions. The second tells
you the answer. Fixed by capturing the child's stderr and quoting it, and by
giving up as soon as the child exits instead of waiting out the full deadline.

**2. Detaching did not release the session — a deadlock, found by the test.**

```
the core never regained the session after detach:
    Ok(Error("a human is attached to this session"))
```

The two pumps block on *different* things: input on the socket, output on the
pty channel. A socket shutdown cannot wake `recv()`. So on detach the output
pump stayed parked on an idle shell, the thread scope never closed, the guard
was never dropped, and the session was locked to a viewer that had already gone
— permanently. Fixed with a shared done-flag and a bounded `recv_timeout`.

Both were live bugs in code that compiled cleanly and looked right. Writing the
expectation down first is what turned "it seems to work" into two specific
failures with addresses.
