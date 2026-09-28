# Deferred command replies

`remuda.pending` is a small asynchronous completion word for extension
commands. It lets an event callback complete a command after its handler has
returned, while the daemon's single Lua thread continues servicing timers and
other work. It composes with asynchronous primitives such as `remuda.http`;
transport and command policy remain separate.

## Create and complete a reply

```lua
local reply = remuda.pending { timeout = 30 }
reply:resolve(value) -- complete successfully with a Lua value
reply:reject(error)  -- complete with a command error
```

The handler returns the pending handle immediately. The caller receives no
reply until the handle resolves or rejects. Returning an ordinary value keeps
the existing synchronous command behavior unchanged. `resolve` values use the
same encoding as ordinary command return values; `reject` takes a descriptive
string and is surfaced as a command failure.

`timeout` is in seconds, must be positive, and is optional. Its default is 30
seconds and the maximum is 300 seconds. The timeout starts when the handler
returns the handle. On expiry, the command completes with a clear timeout
error; completion after expiry is ignored and logged once. A pending handle
must not wait, poll, or otherwise block Lua.

## Completion rules and errors

- A handle accepts one terminal outcome: resolved, rejected, timed out, or
  cancelled because its client went away. A second `resolve` or `reject` is a
  programming error and returns an error without changing the first outcome.
- The reply must fit the existing command reply size limit. An oversized value
  completes with a clear size-limit error; it is not silently cut off.
- An invalid timeout (non-number, non-positive, or above 300 seconds), an
  invalid completion value, or a completion after another terminal outcome is
  reported as a Lua API error. Command rejection, timeout, output-limit
  failure, and client disconnect are distinct from these API misuse errors.
- Handler exceptions retain the command's existing error behavior. A handler
  that returns neither a pending handle nor an ordinary value is subject to
  the existing return-value encoding rules.

The client connection remains associated with the pending reply while the
daemon's connection-handler thread waits on a bounded channel. This wait is
outside the Lua runtime and uses the handle's timeout. When the client
disconnects, the pending entry is removed and marked cancelled; a later
callback completion is a no-op. Completion and timeout races choose exactly
one outcome. Pending state is released after any outcome, so timed-out,
disconnected, and completed commands do not accumulate entries.

## Async callback integration

Async primitives invoke callbacks on the daemon's Lua tick. A callback may
resolve or reject the pending reply there; it only records the result and
signals the waiting connection handler. It must not wait for that handler.
For example, the async-only `remuda.http.request` operation returns a
cancellation handle and later calls its callback on the tick:

```lua
remuda.extension_command("lookup", function(args)
  local url = args[1]
  local reply = remuda.pending { timeout = 35 }
  remuda.http.request { method = "GET", url = url, timeout = 30,
    callback = function(result)
      if result.error then reply:reject(result.error)
      else reply:resolve(result.body) end
    end }
  return reply
end)
```

The HTTP timeout should be shorter than the reply timeout so the callback can
report its result before the command's own deadline. If the command times out
or the client disconnects first, the callback may still arrive; completing a
terminal handle has no effect.

## Command clients

The CLI extension-command caller waits for the deferred result and prints it
using its ordinary success or error behavior. The daemon's Lua event loop
remains available during that wait. MCP callers can use the same semantics
only when their tool invocation is routed through the same extension-command
dispatch and reply path. A separate MCP pending mechanism is outside this API;
if MCP dispatch does not share that path, it remains synchronous until a
separate API is specified.
