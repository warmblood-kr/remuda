# 006 — One living image, and a command loop that is not Emacs's

## Before

정수님, 2026-09-10, describing what he expected to already exist:

> 지금쯤 되어있을 것으로 제가 기대하는 것은, remuda 명령으로 실행 가능. 그러면
> 스탠드얼론 방식이든 데몬 방식이든 구동됨. 그리고 그 오케스트레이터 인스턴스에
> repl을 접근할 수 있음. 거기서, 이름은 다를 수 있지만, open_session(...) 등으로
> 세션을 띄울 수도 있고. 그러면 세션 목록 글로벌 변수에 그 세션 핸들러가 추가되고.
> 세션에 s.insert_char(...) 해서 타이핑을 할 수도 있고. 이 repl은 emacsclient -e
> 처럼, remuda -e 라던지 하는 방식으로 cli에서 트리거할 수도 있고.

Measured against the tree: the first half exists, the second does not — and the
four missing pieces are not four features. They are one absence.

```
remuda 명령 · 스탠드얼론/데몬     ✅
REPL                              ❌
open_session → 전역 세션 목록      ❌  전역이 없다. remuda.ls() 는 매번 «질의»한다
세션 «핸들러»                      ❌  ls() 가 돌려주는 것은 핸들이 아니라 값
s.insert_char(...)                ❌  remuda.insert("name", text) — 매번 이름을 넘긴다
remuda -e                         ❌  remuda run <file.lua> 만 있다
```

The absence is in `script.rs:56` — `Lua::new()`, once per invocation. **The Lua
interpreter lives in the CLI process; the sessions live in the daemon.** Every
binding is therefore an RPC that names a session by string, and nothing Lua
builds can outlive the command that built it. A global session list cannot exist
because there is no global to put it in; a handle cannot exist because there is
nothing for it to be a handle *to* that survives the call.

His model is Emacs: one long-lived image, objects you hold, a REPL that pokes
that image, and `-e` that throws a form at the *same* image from outside. Asked
whether to move there, he answered:

> 네, 이미지 기반이 그 의미라면, 이미지 기반이어야 하겠습니다. 데몬이나
> 스탠드얼론이 떠있는 동안 수명을 함께 하는 lua 메인 프로세스 혹은 쓰레드,
> 혹은 이벤트루프.

So this step moves the interpreter into the daemon. That is the whole of it; the
four features fall out.

### Why this is not a big rewrite

The daemon is already shaped for it, which is worth stating before proposing the
change — the reason to do it now is that the substrate already fits:

- `serve()` holds `Arc<Registry>` and clones it into a thread per connection.
- `Session` locks internally (`Mutex<Box<dyn AgentProcess>>`, `Mutex<Duration>`).

So a Lua thread is one more holder of the same `Arc`. It does not need the socket
protocol at all — it reaches sessions the way the connection threads already do.
The RPC layer stops being Lua's *only* road and becomes what it always should
have been: what the **CLI** speaks, one client among others.

### The shape, and the one place we deliberately differ from Emacs

He offered three: process, thread, or event loop. They are the same answer seen
from three sides, because `mlua::Lua` is a single-threaded state — every Lua
call must be serialized onto one thread, and a thread that serves a queue *is*
an event loop. Emacs reaches the same design for the same reason, and its command
loop is exactly this.

But Emacs pays for it, and 정수님 has been paying for it: a long elisp function
freezes the editor, because the one thread that runs Lisp is also the one that
services everything else. (This tree already carries the diagnosis of one such
freeze.) **We do not have to inherit that**, and the reason is that our sessions
are not Lisp objects — they are Rust structs behind locks, pumped by threads that
never touch Lua:

```
pty reader/writer threads   keep running          ← a slow script does NOT stall output
      │  (lock the Session, as today)
      ▼
  Arc<Registry>  ←──────────  Lua thread (owns Lua, owns the global session table)
      ▲                            ▲
      │                            │  requests over a channel
 connection threads ────────────────┘
 (remuda ls/new/send/…)      (remuda -e / repl / run / mcp)
```

⇒ Lua is serialized; the machine is not. A script that loops forever makes the
*next Lua caller* wait and nothing else. That is strictly better than Emacs, and
it costs us nothing because we were never going to put the pty in Lua's hands.

⚠ It also means a Lua callback cannot be the thing that *drives* a session's
output pump. If we later want `on_output(session, fn)`, the pump must hand work
to the Lua thread, not call into it. Naming that now so it is not discovered as a
deadlock later.

## Desired outcome

**One image.** The daemon owns a `Lua` for its whole life. `remuda -e`, a REPL,
`remuda run`, and `mcp` are four doors into the *same* interpreter, and state set
through one door is visible through the others.

**Handles that are handles.** `open_session` (or whatever we name it — see below)
returns an object, that object goes into a global table the image owns, and
`s:insert("…")` works on it. Because Lua now lives beside the `Registry`, a handle
holds the session, not a name in a costume.

⚠ **A handle must not lie.** The failure mode a proxy invites: the session dies,
the handle does not, and Lua goes on calling methods on a corpse. Two handles to
one session must be the same object, and a dead one must *say* it is dead rather
than fail obscurely. This is the part that has to be designed, not just plumbed —
Emacs's buffer object avoids it by *being* the thing.

**A REPL with a memory.** `remuda repl` and `remuda -e` evaluate in the image, so
a variable set on one line is there on the next. Building either one *without*
the image would have produced something that looks like Emacs and forgets
everything — worse than not building it, because it teaches a false model.

**The CLI keeps working exactly as it does.** `remuda send probe "…"` must not
change behaviour or become slower. It may later be re-expressed as sugar over the
image (`emacsclient -e` style), but not in this step — that is a second move and
it should be made deliberately, with the frozen `v1.lua` spec as the check.

### Naming — a note, not a decision

He wrote `open_session` and `insert_char`, and said the names may differ. Two
observations rather than a ruling:

- `insert_char` in Emacs takes a *codepoint*; `insert` takes a string. We already
  have `insert` (raw bytes) and `key` (named keys) from step 004. If we add
  `insert_char` it should mean what Emacs means, not be a second name for
  `insert`.
- `new` vs `open_session`: `remuda.new` is what step 002 bound and what `v1.lua`
  froze. A rename is a compatibility event, so the handle-returning form should
  be an *addition* — the frozen name keeps its frozen behaviour.

### Deliberately out of scope

- Re-expressing the CLI subcommands as Lua calls (above).
- `on_output` / any Lua callback driven by pty output (above — the pump must not
  call into Lua).
- Multi-image (`remuda -s name`): the socket path already takes a server name,
  so this stays possible without being built.

### Two defects this step's measurement uncovered, both real

Neither is caused by the image move; both were found while probing for it, and
both belong in this step because they are about the daemon's lifetime.

1. **Nothing reaps a superseded daemon.** Auto-start is "if nothing is listening,
   spawn one" — so a rebuild leaves the old daemon running, holding its pty
   children forever. Four were live on the author's machine (below). Whatever
   this step does about the image's lifetime has to answer this too, because an
   image that silently multiplies is worse than no image.
2. **`Request` has no way to close a session.** `List / New / SendLine / Insert /
   …` and no `Kill`. A session can be created and never destroyed. This is a gap
   in step 001, surfaced now; it is small and should be fixed regardless of the
   image.

## Expected

Written before building, so the result can contradict it:

1. The Lua thread will need `Registry` to expose sessions by name without going
   through `Request`/`Response`. I expect that method to already exist or to be a
   few lines, because the connection threads already do exactly this lookup.
2. `mlua::UserData` will carry the handle. I expect `s:insert(text)` to be
   *shorter* than today's `remuda.insert(name, text)` implementation, because the
   name lookup moves from every call to handle construction.
3. The dead-handle question will not be solvable by `UserData` alone — I expect
   to need the handle to hold a weak reference or a generation counter, and to
   discover that the current `Registry` offers neither.
4. `remuda -e "1+1"` will be ~15 lines once the image exists, and ~15 lines
   without it. The difference is entirely in whether the answer persists.
5. `v1.lua` will pass unchanged. If it does not, the image move broke a frozen
   promise and the step is wrong, not the spec.
6. The four-daemon defect will turn out to be a missing check at *start*, not at
   exit — the new daemon should refuse or take over, not let both live.

## Actual

### After building it

Three doors, one interpreter — and each `-e` is a different *process*, which is
the whole claim:

```
$ remuda -e "fleet = {}"          # silent: a statement returns nothing
$ remuda -e "fleet.count = 3"
$ remuda -e "fleet.count"
3
$ remuda -e "1+1"
2
$ remuda -e "'hello' .. ' image'"
hello image
```

The REPL is the same image, and what it changes is there for the next `-e`:

```
$ printf 'fleet.count\nfleet.count = fleet.count + 10\nfleet.count\n' | remuda repl
> 3
> > 13
> $ remuda -e "fleet.count"
13
```

The `remuda` vocabulary is bound inside the image, so sessions are reachable
from it exactly as from a script file:

```
$ remuda -e "remuda.new('imgtest', {'bash'})"
$ remuda -e "#remuda.ls()"
1
$ remuda -e "remuda.ls()[1].name"
imgtest
```

Errors stay errors, with Lua's own traceback:

```
$ remuda -e "nosuchfn()"
remuda: runtime error: (eval):1: attempt to call a nil value (global 'nosuchfn')
stack traceback:
	[C]: in global 'nosuchfn'
	(eval):1: in main chunk
exit=1
```

### Two things the design got wrong, both caught by measuring

**`remuda run` was not a door onto the image.** Desired says "a script file, a
`-e`, and a REPL line are three doors into one interpreter" — but `script::run`
still built its own `Lua::new()` in the *client* process, and I did not notice
until a script asked for a table two `-e` calls had just built:

```
$ remuda -e "fleet = {count = 7}"
$ cat img.lua
print("script sees fleet:", tostring(fleet))
$ remuda run img.lua
script sees fleet:	nil          ← its own interpreter, not the image
```

Fixed by making `run` read the file and send it as `Request::Eval`, carrying
the path so a traceback still names it. `Request::Eval` grew a `name` field for
exactly that — the alternative was letting every script error report `(eval)`,
trading this repo's own stated care about chunk names for one fewer field.
Re-measured:

```
$ remuda -e "fleet = {count = 7}"
$ remuda run img.lua
script sees fleet.count:	7
$ remuda -e "fleet.count"        # the script doubled it
14
$ remuda run boom.lua
remuda: runtime error: …/scratchpad/boom.lua:2: deliberate    ← file still named
```

**`print` went to `/dev/null`.** The daemon is spawned with `Stdio::null()`, so
`print` inside the image wrote to a stdout nobody holds. Nothing errored;
output simply vanished. `print` is the first thing anyone types into a scratch
buffer, so this made the feature look broken while being technically correct.
Fixed by rebinding `print` to a per-job buffer that travels back with the
result:

```
$ remuda -e 'print("hello from image")'
hello from image
$ remuda -e 'do print("side effect") end return 99'
side effect
99
$ remuda -e "y = 1"              # still silent — printed nothing, returned nothing
```

`nil` also rendered as `<nil>` through a type-name fallback, which is not Lua
and reads like a placeholder that failed to fill in. Now:

```
$ remuda -e "nil"      nil
$ remuda -e "true"     true
$ remuda -e "({})"     <table>    ← type without the address, so output is reproducible
```

### Confirmations, and the suite

Expected #1 held — `mlua::Lua` is `!Send` without the `send` feature, so the
"process / thread / event loop" fork 정수님 offered was never three options:
the state is pinned to one thread and reached by channel, which *is* an event
loop. No new dependency, no feature flag.

Expected #5 held — `v1.lua` passes unchanged; `Eval` is an addition.

```
$ cargo test --release -q
test result: ok. 21 passed   (invariants)
test result: ok. 10 passed
test result: ok. 1 passed
test result: ok. 4 passed
test result: ok. 8 passed
test result: ok. 4 passed
..v1 ok
test result: ok. 4 passed    ← the frozen compatibility spec
$ cargo clippy --release --all-targets -q                       (clean)
$ cargo fmt --check                                             (clean)
$ cargo check -p remuda-core --target wasm32-unknown-unknown    (clean)
$ python3 scripts/check-principles.py
ok — 10 principles, every named mechanism exists (7 CI jobs, 11 denied paths, 101 test fns seen)
```

### What this step did NOT do

Expected #2/#3 — session handles as `UserData`, a global session table, the
dead-handle question — are **not built**. The image exists and everything
persists in it, but `remuda.ls()` still returns values and operations still
name a session by string. That was the larger half of Desired and it is
deferred deliberately: 정수님's follow-up — *"어디서든 내부 런타임에 코드를
전달하여 실행"* — is about reaching the runtime, and that is what shipped.
Handles carry a real design question (a proxy that outlives its session is a
handle that lies) and folding them in here would have made this step
unreviewable.

Also unbuilt, and named in Desired: re-expressing the CLI subcommands as image
calls. `remuda send` still speaks the protocol directly, so the two paths could
in principle drift — the frozen `v1.lua` is what would catch it.

⚠ A cost this step accepted knowingly: the image's `remuda` table talks to the
daemon over its **own socket** rather than reaching the `Registry` directly as
the design predicted. One definition of the vocabulary instead of two that
could diverge, at the price of a loopback hop per call. It cannot deadlock —
the daemon answers each connection on its own thread — but it is not what the
design said, and the direct path is the obvious thing to take when handles
arrive.

### Measurements taken before any of the above was designed

Commands as run, output as returned.

The binary works standalone, with no manual daemon start:

```
$ cargo build --release -q
$ ./target/release/remuda ls
no sessions
exit=0
$ ./target/release/remuda new probe -- bash
new exit=0
$ ./target/release/remuda ls
probe                  80x24   live  idle 1s
ls exit=0
$ ./target/release/remuda send probe "echo 안녕 \$((6*7))"
$ ./target/release/remuda capture probe | tail -4
toracle@jarvice:~/projects/pty-core-wip$ echo 안녕 $((6*7))
안녕 42
toracle@jarvice:~/projects/pty-core-wip$
```

The Lua state is per-invocation — this is the sentence the whole step turns on:

```
$ sed -n '54,58p' native/src/script.rs
pub fn run(socket: &Path, script: &Path) -> mlua::Result<()> {
    let source = std::fs::read_to_string(script)?;
    let lua = Lua::new();
    let table = bindings(&lua, socket)?;
    lua.globals().set("remuda", table)?;
```

Everything Lua is given, in full — nine functions on one table, no objects:

```
$ grep -A1 'table.set(' native/src/script.rs | grep '"' | tr -d ' ",'
ls
new
send
insert
key
click
capture
attach
sleep
```

Searched for the pieces 정수님 described, with a positive control so a zero is a
real zero and not a broken search:

```
$ for k in UserData insert_char open_session repl_ interactive; do
    printf "  %-14s %s\n" "$k" "$(grep -rn -- "$k" --include='*.rs' --include='*.lua' . | grep -v /target/ | wc -l)"
  done
  UserData       0
  insert_char    0
  open_session   0
  repl_          0
  interactive    0

  # controls, same command, terms that must appear
  capture        66
  mlua           17
```

The daemon already has the shape this step needs — `Arc<Registry>`, a thread per
connection, sessions locked internally:

```
$ sed -n '52,60p' native/src/daemon.rs
    let registry = Arc::new(Registry::new());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || {
            let _ = handle(stream, &registry);
        });
    }

$ grep -n 'Mutex' core/src/session.rs | head -4
13:use std::sync::{Arc, Mutex};
56:    agent: Mutex<Box<dyn AgentProcess>>,
61:    last_input_at: Mutex<Duration>,
```

Defect 1 — four daemons alive, one socket:

```
$ ps -eo pid,etime,cmd | grep '[r]emuda daemon'
 200999    02:57:50 target/debug/remuda daemon
2361813    07:00:50 target/debug/remuda daemon
2611260    06:31:05 target/debug/remuda daemon
1696620       00:28 target/release/remuda daemon    ← the one holding the socket
$ ls -la $XDG_RUNTIME_DIR/remuda/
srwxr-xr-x 1 toracle toracle 0 Sep 10 08:11 default.sock
```

Defect 2 — the protocol can create a session and cannot end one:

```
$ grep -n -A20 'pub enum Request' core/src/protocol.rs | grep -E '^\s*[0-9]+-\s+[A-Z]'
26-    List,
28-    New {
35-    SendLine { name: String, text: String },
    (…Insert, Key, Click, Capture, Attach, Run…)
    # no Kill, no Close — searched, absent
```
