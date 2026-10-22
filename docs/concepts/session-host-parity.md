# Host-backed vs in-process sessions: known differences (#713)

Host-backed sessions (`REMUDA_EXPERIMENTAL_SESSION_HOST=1`) aim to behave
exactly like in-process ones. Parity tests run each scenario against both
backends (`native/tests/hosted_parity*.rs`, harness
`native/tests/daemon_support/parity.rs`). What still differs, on purpose or
for now:

| Difference | Why | Caller sees |
|---|---|---|
| An input act (send, send_line, type_text, feed, clear) is at most 8 KiB | The host inventory stores every act in full and validates it on load; a larger act would make an older build reject the whole inventory, which breaks reload rollback | `hosted input acts are limited to 8 KiB; split the message` |
| Launch limits: name 64 bytes; command 4096 arguments / 512 KiB; working directory 1024 bytes; environment 4096 variables, 128 KiB per value, 512 KiB in all | The inventory validates every name it holds, with the same rollback concern; the rest bound what a launch commits to disk and memory | `host-backed launch refused: ...` naming the limit |
| The runtime directory needs 23 more bytes of path | A host's socket sits under `<socket>.hosts/<id>/` | `session host socket path too long`; the default `/tmp/remuda-$USER` is far below it |
| Attach shows the host's parsed screen, not the child's raw bytes | A host keeps typed screens, no raw output stream | bell, window title, OSC 52 clipboard, OSC 8 links and image protocols do not reach the viewer; modes (mouse, paste, cursor keys) do |
| Terminal size at most 65,536 cells | Host terminal budget | very large terminals (e.g. 300x250) are refused |
| Sequenced remote input forgets duplicates across a daemon restart | Duplicate memory lives in the daemon, as for an in-process session, which would not survive the restart at all | a retry spanning a restart or reload is not recognised as a duplicate |
| A session survives the daemon | The point of hosting | sessions are still listed after a daemon restart or reload |
| Windows | No host there (#214) | in-process only |

An unreachable host is listed with `unavailable: true` (`remuda ls`:
"unreachable") instead of looking like a healthy session.

Open item for making hosting the default: large hosted input. Options: allow
large acts and have reload refuse to roll back while such records exist, or
send large text through the human raw input path (ordered, not stored, no
submit verification).

## Owner decision: orphaned hosts

A host whose private directory (and so its socket) has been deleted can never
be reached, adopted or closed by any daemon again. It keeps its PTY and its
program running until the program exits on its own. Tests used to leak
hundreds of these per run; their fixtures now stop their own hosts
(`native/tests/daemon_support/hosts.rs`), so the product is unchanged.

Option: such a host ends itself and its program. That frees PTYs and stops
programs nobody can reach. The risk is ending a user's program, perhaps
mid-task, because a directory disappeared: a /tmp cleaner, a mistaken `rm`, a
cache sweep. Mitigations, all required together:
- the endpoint directory is gone, not merely unreachable;
- it has been gone for N minutes (e.g. 30), re-checked;
- the program has been idle (no output) for that whole time.

Not done: ending users' programs is the owner's call.
