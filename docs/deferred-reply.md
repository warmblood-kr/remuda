# Deferred command replies

`remuda.pending` is a small asynchronous completion word for extension
commands. It lets an event callback complete a command after its handler has
returned, while the daemon's single Lua thread continues servicing timers and
other work. It composes with asynchronous primitives such as `remuda.http`;
transport and command policy remain separate.

## Create and complete a reply

```lua
local reply = remuda.pending { timeout = 30 }
reply:resolve(code, stdout, stderr) -- complete with a command result
reply:reject(error)                 -- complete with a command failure
```

The handler returns the pending handle immediately. The caller receives no
reply until the handle resolves or rejects. A resolved command result is the
tuple `(exit_code, stdout_bytes, stderr_bytes)`. The exit code is an integer
from 0 through 255; stdout and stderr are Lua byte strings. The CLI writes each
byte string verbatim to its corresponding stream and exits with the supplied
code. In particular, `--json` output belongs on stdout and errors belong on
stderr with a non-zero code. `reject(error)` takes a descriptive string and
surfaces a command failure through the ordinary CLI error path. Returning an
ordinary value remains a shorthand for today's synchronous success behavior.
The daemon encodes stdout and stderr as Base64 strings in its JSON protocol;
the CLI decodes them and writes the original bytes.

`timeout` is in seconds, must be positive, and is optional. Its default is 30
seconds and the maximum is 300 seconds, which allows a 60 second upload to
finish with time for the result callback. The timeout starts when the handler
returns the handle. On expiry, the command completes with a clear timeout
error; completion after expiry is ignored and logged once. A pending handle
must not wait, poll, or otherwise block Lua.

An optional `on_cancel(reason)` callback lets a handler cancel work when the
client disconnects, the handle times out, or the daemon shuts down. `reason` is
`"client_disconnected"`, `"timeout"`, or `"shutdown"`. It runs at most once on
the Lua tick, never on the connection handler thread, and is not called after
the reply has resolved or been rejected. The handle's timeout error is still
sent to a connected client when timeout causes cancellation.

## Completion rules and errors

- A handle accepts one terminal outcome: resolved, rejected, timed out, or
  cancelled because its client went away. A second `resolve` or `reject` is a
  programming error and returns an error without changing the first outcome.
- A deferred result's combined stdout and stderr must be at most
  `MAX_REPLY_BYTES` (16 MiB). Oversized `resolve` calls raise a clear
  size-limit error to Lua and fail the deferred request; they are not silently
  cut off. The same logical content limit applies to synchronous string
  replies. Serialized wire frames have a bounded allowance for JSON escaping
  and base64 encoding.
- An invalid timeout (non-number, non-positive, or above 300 seconds), an
  invalid exit code or output type, a missing result field, or completion after
  another terminal outcome is reported as a Lua API error. Command rejection,
  timeout, output-limit failure, and client disconnect are distinct from these
  API misuse errors.
- Handler exceptions retain the command's existing error behavior. A handler
  that returns neither a pending handle nor an ordinary value is subject to
  the existing return-value encoding rules.

The client connection remains associated with the pending reply while the
daemon's connection-handler thread waits on a bounded channel. This wait is
outside the Lua runtime and uses the handle's timeout. When the client
disconnects, the pending entry is removed and marked cancelled, and
`on_cancel("client_disconnected")` is queued on the Lua tick; a later callback
completion is a no-op. Timeout similarly queues `on_cancel("timeout")` once.
Completion and timeout/disconnect races choose exactly one outcome. Pending
state is released after the outcome and cancellation notification are handed
off, so timed-out, disconnected, and completed commands do not accumulate
entries.

## Async callback integration

Async primitives invoke callbacks on the daemon's Lua tick. A callback may
resolve or reject the pending reply there; it only records the result and
signals the waiting connection handler. It must not wait for that handler.
For example, the async-only `remuda.http.request` operation returns a
cancellation handle and later calls its callback on the tick:

```lua
remuda.extension_command("lookup", function(args)
  local request
  local reply = remuda.pending { timeout = 90, on_cancel = function()
    if request then request:cancel() end end }
  request = remuda.http.request { method = "GET", url = args[1], timeout = 60,
    callback = function(result)
      if result.error then reply:resolve(1, "", result.error)
      else reply:resolve(0, result.body, "") end end }
  return reply
end)
```

The HTTP timeout should be shorter than the reply timeout so the callback can
report its result before the command's own deadline. If the command times out
or the client disconnects first, `on_cancel` cancels the HTTP operation; any
already queued callback that attempts to complete a terminal handle has no
effect.

## Capacity, shutdown, and module lifecycle

At most 64 pending replies may exist per daemon. Creating another handle over
the cap fails immediately with a clear capacity error; it does not wait or
queue without a bound. Each pending reply owns a waiting connection-handler
thread, which is why this limit is separate from the per-handle timeout.

On daemon shutdown, each still-connected pending caller receives a clear
`daemon stopping` error. The daemon queues `on_cancel("shutdown")` for each
such handle on the Lua tick as a best effort before stopping the Lua runtime.
Shutdown does not block indefinitely to run callbacks; cancellation delivery
has a bounded drain period.

A pending handle remains valid if the module that created it is stopped or
reloaded. Its captured closures may still resolve or reject it, and it remains
subject to its timeout and client lifecycle. Module stop or reload does not
silently discard or extend the pending reply.

## Prompts

While a reply is pending, the handle can ask the caller's terminal one
question at a time: `reply:prompt_secret { label, callback }` or
`reply:prompt_line { label, default?, preface?, callback }`.

- The label is one short line. Control characters (newlines included) and
  invisible formatting characters are removed, and the caller's text is cut at
  256 characters. The daemon prefixes it with its own tag, `remuda[outside]` or
  `remuda[session NAME]`, and the CLI appends ` [default]` and `: `. The CLI
  keeps the prompt on one terminal row and clips its displayed text to the
  terminal's display width, marking a clipped prompt with `…`. Pressing Enter
  still returns the full sanitized default.
- `preface` is optional text shown above the prompt, for example a summary
  before `Continue?`. Lines are separated by `\n`; one trailing newline is
  ignored. Each line goes through the same sanitizer as the label and is
  printed on stderr, indented by two spaces and without a tag, so a preface
  line can never pass for the daemon's prompt line. A line that is empty after
  sanitizing keeps its row.
- A preface may have at most 32 lines of at most 256 characters each. Over a
  limit, `prompt_line` raises an error; nothing is cut silently.
- A CLI older than the daemon ignores the preface and shows only the prompt.
  On a daemon older than this feature the field is ignored, so keep the label
  meaningful on its own.

## Command clients

The CLI extension-command caller waits for the deferred result and prints it
using its ordinary success or error behavior. The daemon's Lua event loop
remains available during that wait. MCP tool calls currently enter
`remuda._call`, not `remuda._dispatch_extension_command`, so they remain
synchronous. A separate MCP pending mechanism is outside this API.
