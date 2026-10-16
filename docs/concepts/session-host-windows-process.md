# Windows creation/wait preparation for #713

This preparation does not enable the standalone host or daemon adoption on
Windows. Its executable still refuses before spawning there. Named-pipe peer
authentication, ConPTY host ownership/drain, Job lifetime, full operations and
recovery must be implemented and independently reviewed before support claims.

The pinned MIT portable-pty0.9.0 source is under vendor/portable-pty with upstream
archive checksum/VCS/license and patch rationale in REMUDA-PATCH.md. The optional
Windows remuda-job-list feature adds SlavePty::spawn_command_in_job. ConPTY uses
two STARTUPINFOEX attributes: PSEUDOCONSOLE and JOB_LIST in the same CreateProcessW.
It reuses upstream Windows command quoting/environment/cwd, creates no bootstrap
helper, inherits no Job handle, and never retries uncontained after failure.
Ordinary spawn has no Job attribute; Unix implementation is unchanged.

Contract: [Microsoft UpdateProcThreadAttribute](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute).
JOB_LIST is available on Windows10+/Server2016+; its HANDLE array remains live
until attribute-list destruction. HANDLE_LIST inheritance rules are different.
Incompatible enclosing Job restrictions must remain errors, not bypasses.

Experimental windows_process::Process retains the real process handle inside
one private portable Child owner. It never calls upstream kill/try_wait/wait/
Future: 0.9.0 reverses/masks termination errors, conflates actual exit259 with
STILL_ACTIVE, hides query failures and ignores wait results; its Future also
does not move the owned cloned handle into the waiter. The wrapper first checks
WAIT_OBJECT_0/WAIT_TIMEOUT/WAIT_FAILED, then queries the exit code only after a
signaled wait. Exit259 is final. Termination success only initiates termination;
a timeout yields pending and OS errors stay errors. A failed terminate racing
natural exit succeeds only after a checked signaled wait plus exit query.

The caller owns a configured noninheritable kill-on-last-close/no-breakaway Job,
retains it for the child/descendant lifetime, and drops/terminates it on every
launch error. The wrapper checks membership using the returned real handle, not
PID reopen; that check is a postcondition, never a substitute for creation-time
assignment. A failed postcondition attempts checked bounded direct-child cleanup
and reports its outcome. An error/timeout does not prove process termination.
Direct-child status, descendant Job lifetime and ConPTY EOF/close remain separate.

Native-only synthetic tests use a named fixture Job solely to observe exact
membership before the fixture writes markers or spawns an immediate grandchild.
The child closes its temporary query handle before either action; no Job handle
is inherited. They exercise Unicode/empty/quoted argv/cwd/env, exit259, 2MiB
output with an independent bounded-memory drain, grandchild survival after direct
exit and termination on Job last-close, outside-child isolation, native direct
termination and a valid non-Job handle that must refuse without fallback.
Fault injection covers pending/failed waits, query failure and terminate races;
native invalid-handle tests exercise real OS error returns. Cross compilation
and Unix passes cannot establish these native lifecycle assertions.

The first native run passed the three wait/error unit tests but both process
fixtures timed out. The synthetic harness now asynchronously answers initial
cursor inheritance queries through a four-item reply queue, with a 512-byte
diagnostic tail on failure. It is not the host terminal parser. Upstream enables
PSEUDOCONSOLE_INHERIT_CURSOR; [Microsoft documents the async reply requirement](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole).
Native Windows rerun at088728db passed all3 unit and2 process fixtures; all24 CI
checks were green before normal reviewed mergeacff601a. The responder change
resolved the fixture timeout; a unique cursor-query cause was not proved by
observed success counts or a controlled reversal. This remains process preparation,
not actual Windows host support or a host-death/controller-death proof.

Remaining Windows gates include actual stable host+authenticated named pipes,
controller SIGKILL vs host death before/during/after spawn, enclosing Job failure,
all creation/thread/fd/handle cleanup injection, CtrlC, ClosePseudoConsole during
concurrent final output, bounded cancellation/deadlines/queues, aggregate fairness,
no leaked reader threads or handles, and actual native CI on reviewed exact heads.
No production launch/restart or existing-session migration is part of this work.
