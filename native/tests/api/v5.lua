-- v5 accumulates new words until the next tagged release freezes it. v1-v4
-- remain frozen; this version introduced nested session words and retains the
-- callable constructor.

local session = remuda.session
assert(type(session) == "table", "remuda.session must be a namespace table")
assert(getmetatable(session) and type(getmetatable(session).__call) == "function",
  "remuda.session must remain callable")
for _, word in ipairs({ "list", "new", "close", "attach" }) do
  assert(type(session[word]) == "function", "remuda.session." .. word .. " is missing")
end

local name = "api-v5-" .. tostring(os.time())
local opened = session.new(name, { "sh" })
assert(opened == name, "session.new must preserve new's return value")
local found = false
for _, row in ipairs(session.list()) do
  if row.name == name then found = true end
end
assert(found, "session.list must include the session created by session.new")

local handle = session(name)
assert(handle.name == name, "calling remuda.session must still return a handle")
assert(type(handle.is_busy) == "boolean", "the callable table must preserve session handle properties")
assert(session.close(name) == nil, "session.close must preserve close's return value")

-- Flat spellings remain usable through API v5 while emitting suppressible
-- deprecation notices for callers that have not migrated yet.
local legacy_name = "api-v5-legacy-" .. tostring(os.time())
remuda.new(legacy_name, { "sh" })
local legacy_found = false
for _, row in ipairs(remuda.ls()) do
  if row.name == legacy_name then legacy_found = true end
end
assert(legacy_found, "deprecated flat session aliases must remain compatible in v5")
remuda.close(legacy_name)

-- v5 also carries the bounded synchronous process word until the next API
-- version is frozen. It executes argv directly, with bounded time and output.
assert(type(remuda.process) == "table", "remuda.process must be a callable namespace table")
local process_mt = getmetatable(remuda.process)
assert(process_mt and type(process_mt.__call) == "function",
  "remuda.process must preserve the asynchronous process(spec) call")
assert(type(remuda.process.run) == "function", "remuda.process.run is missing")

local windows = package.config:sub(1, 1) == "\\"
local echo_argv = windows
  and { "cmd.exe", "/c", "echo", "remuda-process-run-v5" }
  or { "/bin/echo", "remuda-process-run-v5" }
local result = remuda.process.run({ argv = echo_argv, timeout = 3 })
assert(result.code == 0, "process.run should return the child exit code")
assert(result.stdout:find("remuda-process-run-v5", 1, true), "process.run should capture stdout")
assert(result.stderr == "", "process.run should capture stderr separately")
assert(result.timed_out == false, "a completed process must not be marked timed out")

local slow_argv = windows
  and { "ping.exe", "-n", "30", "127.0.0.1" }
  or { "/bin/sleep", "10" }
local timed = remuda.process.run({ argv = slow_argv, timeout = 0.2 })
assert(timed.timed_out, "process.run must kill a child when its timeout expires")
assert(timed.code == 124, "timed-out process.run must return timeout code 124")
assert(type(remuda.session.list()) == "table", "the daemon should continue handling Lua work after timeout")

local bad_timeout = pcall(function()
  remuda.process.run({ argv = echo_argv, timeout = 31 })
end)
assert(not bad_timeout, "process.run must reject a timeout above the 30-second hard cap")

print("v5 ok")
