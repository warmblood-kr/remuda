-- v5 adds the session namespace and deferred replies for extension commands.
-- v1-v4 remain frozen; v5 remains open until the next tagged release.
remuda._api_v5_exit_events = {}
remuda._api_v5_legacy_exit_names = {}
remuda._api_v5_output_events = {}
remuda._api_v5_legacy_output_names = {}
remuda.on("session_exited", function(name, details)
  table.insert(remuda._api_v5_exit_events, { name = name, details = details })
end)
remuda.on("session_exited", function(name)
  table.insert(remuda._api_v5_legacy_exit_names, name)
end)
remuda.on("session_output", function(name, details)
  table.insert(remuda._api_v5_output_events, { name = name, details = details })
end)
remuda.on("session_output", function(name)
  table.insert(remuda._api_v5_legacy_output_names, name)
end)

function remuda._api_v5_output_seen(name)
  for _, event in ipairs(remuda._api_v5_output_events) do
    if event.name == name then return true end
  end
  return false
end

function remuda._api_v5_assert_output(name)
  local event
  for _, candidate in ipairs(remuda._api_v5_output_events) do
    if candidate.name == name then event = candidate; break end
  end
  assert(event, "session_output did not include " .. name)
  assert(type(event.name) == "string" and event.name == name,
    "session_output's first argument must be the session name")
  assert(type(event.details) == "table", "session_output must provide a details table")
  assert(type(event.details.version) == "number" and event.details.version > 0,
    "session_output details.version must be a positive number")
  local legacy_received_name = false
  for _, legacy_name in ipairs(remuda._api_v5_legacy_output_names) do
    if legacy_name == name then legacy_received_name = true; break end
  end
  assert(legacy_received_name, "one-argument session_output handlers must receive the name")
end

function remuda._api_v5_exit_seen(name)
  for _, event in ipairs(remuda._api_v5_exit_events) do
    if event.name == name then return true end
  end
  return false
end

function remuda._api_v5_assert_exit(name, reason, exit_code, signal, signal_name)
  local event
  for _, candidate in ipairs(remuda._api_v5_exit_events) do
    if candidate.name == name then event = candidate; break end
  end
  assert(event, "session_exited did not include " .. name)
  assert(type(event.name) == "string" and event.name == name,
    "session_exited's first argument must remain the session name")
  assert(type(event.details) == "table", "session_exited must provide its details as argument two")
  assert(event.details.reason == reason,
    "unexpected session_exited reason: " .. tostring(event.details.reason))
  if exit_code ~= nil then
    assert(event.details.exit_code == exit_code,
      "unexpected session_exited exit code: " .. tostring(event.details.exit_code))
  end
  if signal ~= nil then
    assert(event.details.signal == signal,
      "unexpected session_exited signal: " .. tostring(event.details.signal))
  end
  if signal_name ~= nil then
    assert(event.details.signal_name == signal_name,
      "unexpected session_exited signal name: " .. tostring(event.details.signal_name))
  end
  if event.details.signal ~= nil then
    assert(type(event.details.signal) == "number", "session_exited signal must be a number")
  end
  if event.details.signal_name ~= nil then
    assert(type(event.details.signal_name) == "string", "session_exited signal_name must be a string")
    assert(type(event.details.signal) == "number", "signal_name requires a numeric signal")
  end
  if reason == "closed" then
    assert(event.details.signal == nil and event.details.signal_name == nil,
      "closed sessions must omit signal details")
  end
  local legacy_received_name = false
  for _, legacy_name in ipairs(remuda._api_v5_legacy_exit_names) do
    if legacy_name == name then legacy_received_name = true; break end
  end
  assert(legacy_received_name, "one-argument session_exited handlers must keep receiving the name")
end

assert(type(remuda.pending) == "function", "remuda.pending is missing")
assert(remuda._registry.pending ~= nil, "remuda.pending needs a registry entry")
assert(remuda._pending_replies == nil, "pending manager internals must remain private")

local input = remuda.input
assert(type(input) == "table", "remuda.input is missing")
assert(type(input.text) == "function", "remuda.input.text is missing")
assert(type(input.submit) == "function", "remuda.input.submit is missing")
assert(type(input.type_text) == "function", "remuda.input.type_text is missing")
assert(remuda._registry["input.text"] ~= nil, "remuda.input.text needs a registry entry")
assert(remuda._registry["input.submit"] ~= nil, "remuda.input.submit needs a registry entry")
assert(remuda._registry["input.type_text"] ~= nil, "remuda.input.type_text needs a registry entry")

local input_name = "api-v5-input-" .. tostring(os.time())
remuda.session.new(input_name, { "sh" })
local command = "printf INPUT_UNIT_V5_OK"
input.text(input_name, command)
input.submit(input_name, command)
local input_ready = false
for _ = 1, 40 do
  if remuda.capture(input_name):find("INPUT_UNIT_V5_OK", 1, true) then
    input_ready = true
    break
  end
  remuda.sleep(0.05)
end
assert(input_ready, "input.text + input.submit must execute a plain shell command")
remuda.session.close(input_name)

local bad_timeout = pcall(remuda.pending, { timeout = 301 })
assert(not bad_timeout, "pending timeout must not exceed 300 seconds")

local session = remuda.session
assert(type(session) == "table", "remuda.session must be a namespace table")
assert(getmetatable(session) and type(getmetatable(session).__call) == "function",
  "remuda.session must remain callable")
for _, word in ipairs({ "list", "new", "close", "attach", "resize" }) do
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
assert(session.resize(name, 91, 31) == true, "session.resize must report success")
local resized = false
for _, row in ipairs(remuda.ls()) do
  if row.name == name then resized = row.cols == 91 and row.rows == 31 end
end
assert(resized, "session.resize must update dimensions reported by remuda.ls")
assert(session.resize(name, 30, 24) == true, "session.resize must accept a pane-width session")
local narrow = false
for _, row in ipairs(remuda.ls()) do
  if row.name == name then narrow = row.cols == 30 and row.rows == 24 end
end
assert(narrow, "session.resize must preserve sub-80 widths reported by remuda.ls")
for _, dimensions in ipairs({
  { 0, 24 }, { 19, 24 }, { 1001, 24 }, { 80, 0 }, { 80, 23 }, { 80, 501 },
}) do
  local ok, err = session.resize(name, dimensions[1], dimensions[2])
  assert(ok == nil and type(err) == "string", "session.resize must reject out-of-bounds dimensions")
end
local nonnumeric, nonnumeric_err = session.resize(name, "80", 24)
assert(nonnumeric == nil and type(nonnumeric_err) == "string", "session.resize must reject nonnumeric dimensions")
local unknown, unknown_err = session.resize(name .. "-missing", 90, 30)
assert(unknown == nil and type(unknown_err) == "string", "session.resize must report an unknown session")

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

local missing_program = "remuda-process-run-missing-executable-267"
local missing_ok, missing_error = pcall(function()
  remuda.process.run({ argv = { missing_program }, timeout = 1 })
end)
assert(not missing_ok, "process.run should fail for a missing executable")
assert(tostring(missing_error):find(missing_program, 1, true),
  "missing executable error should name the program")

local async_id = remuda.process({ argv = echo_argv })
assert(type(async_id) == "number", "the callable process namespace must preserve process(spec)")
if not windows then
  local capped_output = remuda.process.run({
    argv = { "/usr/bin/head", "-c", "1048577", "/dev/zero" }, timeout = 3,
  })
  local marker = "\n[output truncated by remuda.process.run]"
  assert(#capped_output.stdout == 1048576 + #marker,
    "the output cap should retain 1 MiB of data before the truncation marker")
  assert(capped_output.stdout:byte(1) == 0 and capped_output.stdout:byte(1048576) == 0,
    "the truncation marker must not consume bytes from the 1 MiB payload cap")
  assert(capped_output.stdout:sub(-#marker) == marker, "the truncation marker should be appended")

  local piped = remuda.process.run({ argv = { "/bin/cat" }, stdin = "process stdin v5", timeout = 3 })
  assert(piped.stdout == "process stdin v5", "process.run should pass stdin to the child")
end

local slow_argv = windows
  and { "ping.exe", "-n", "30", "127.0.0.1" }
  or { "/bin/sleep", "10" }
local timed = remuda.process.run({ argv = slow_argv, timeout = 0.2 })
assert(timed.timed_out, "process.run must kill a child when its timeout expires")
assert(timed.code == 124, "timed-out process.run must return timeout code 124")
if not windows then
  assert(timed.signal == 9, "process.run should report Unix SIGKILL when timeout kills the child")
  -- The shell exits naturally, but its background child inherits stdout and
  -- stderr. Preserve the leader's status and kill the remaining process group.
  for _ = 1, 20 do
    local started = os.time()
    local held_pipes = remuda.process.run({
      argv = { "/bin/sh", "-c", "sleep 30 & echo $!; exit 0" }, timeout = 3,
    })
    local held_pid = held_pipes.stdout:match("(%d+)")
    assert(not held_pipes.timed_out, "a naturally exited leader must not be reported timed out")
    assert(held_pipes.code == 0, "process.run must report the leader's zero exit code")
    assert(held_pid, "the pipe-holding descendant pid should be captured")
    assert(os.time() - started < 3, "process.run should drain after killing the leader's process group")
    local child_alive = os.execute("/bin/kill -0 " .. held_pid .. " >/dev/null 2>&1")
    assert(child_alive ~= true and child_alive ~= 0,
      "the background child should be gone after process.run returns")
    local after_group_cleanup = remuda.process.run({ argv = echo_argv, timeout = 1 })
    assert(after_group_cleanup.code == 0, "group cleanup should release output-reader permits")
  end

  -- A descendant can escape the process group with setsid and keep both
  -- output pipes alive. Use -f explicitly: util-linux setsid otherwise forks
  -- only when its caller is already a process-group leader. Keep the original
  -- leader alive until process.run times out, so the assertion does not depend
  -- on a scheduling race. Limit detached readers so repeated calls cannot leak
  -- unbounded threads and file descriptors. Skip systems without setsid -f.
  local setsid_probe = pcall(function()
    local probe = remuda.process.run({ argv = { "setsid", "-f", "/bin/true" }, timeout = 1 })
    assert(probe.code == 0, "setsid probe failed")
  end)
  if setsid_probe then
    local escaped_pids = {}
    local exercised_cap, cap_error = pcall(function()
      for _ = 1, 8 do
        local escaped = remuda.process.run({
          argv = { "/bin/sh", "-c", "setsid -f /bin/sh -c 'echo $$; exec /bin/sleep 30' & exec /bin/sleep 30" },
          timeout = 0.1,
        })
        assert(escaped.timed_out, "setsid descendant should leave output pipes open")
        local pid = escaped.stdout:match("(%d+)")
        assert(pid, "setsid descendant pid should be captured")
        escaped_pids[#escaped_pids + 1] = pid
      end
      local capped, refusal = pcall(function()
        remuda.process.run({ argv = echo_argv, timeout = 1 })
      end)
      assert(not capped, "process.run must refuse calls after reaching the detached reader cap")
      assert(tostring(refusal):find("limit of 16 output-reader workers", 1, true),
        "reader cap should explain why the call was refused")
    end)
    for _, pid in ipairs(escaped_pids) do os.execute("/bin/kill -KILL " .. pid) end
    local recovered = false
    for _ = 1, 40 do
      os.execute("/bin/sleep 0.05")
      local ok = pcall(function() remuda.process.run({ argv = echo_argv, timeout = 1 }) end)
      if ok then recovered = true; break end
    end
    assert(exercised_cap, tostring(cap_error))
    assert(recovered, "reader permits should be released after escaped descendants exit")
  end
end
assert(type(remuda.session.list()) == "table", "the daemon should continue handling Lua work after timeout")

local bad_timeout = pcall(function()
  remuda.process.run({ argv = echo_argv, timeout = 31 })
end)
assert(not bad_timeout, "process.run must reject a timeout above the 30-second hard cap")
local absent_readiness = remuda._module_readiness("api-v5-no-ready-declaration")
assert(absent_readiness.status == "ready",
  "a missing readiness declaration must preserve immediate completion")
local json = remuda.json
assert(type(json) == "table", "remuda.json is missing from the v5 surface")
local decoded, decode_error = json.decode('{"values":[true,null]}')
assert(decoded and decode_error == nil, tostring(decode_error))
assert(json.encode(decoded) == '{"values":[true,null]}', "json encode/decode must round-trip")
local duplicate, duplicate_error = json.decode('{"key":1,"key":2}')
assert(duplicate == nil and duplicate_error == "duplicate key", "duplicate JSON keys must be rejected")
local too_deep = string.rep("[", 65) .. "0" .. string.rep("]", 65)
local limited, limit_error = json.decode(too_deep)
assert(limited == nil and type(limit_error) == "string", "JSON depth limit must be enforced")

local fs = remuda.fs
assert(type(fs) == "table", "remuda.fs is missing")
assert(type(fs.write_atomic) == "function", "remuda.fs.write_atomic is missing")
local write_path = os.tmpname()
os.remove(write_path)
local wrote, write_error = fs.write_atomic(write_path, "first\0record")
assert(wrote == true and write_error == nil, tostring(write_error))
wrote, write_error = fs.write_atomic(write_path, "replacement")
assert(wrote == true and write_error == nil, tostring(write_error))
local written = assert(io.open(write_path, "rb"))
assert(written:read("*a") == "replacement", "atomic write must replace an existing file")
written:close()
os.remove(write_path)
local failed, file_error = fs.write_atomic(write_path .. ".missing/child", "unwritable")
assert(failed == nil and type(file_error) == "string", "write errors must return nil, error")

print("v5 ok")
