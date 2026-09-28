-- v5 adds the session namespace and deferred replies for extension commands.
-- v1-v4 remain frozen; v5 remains open until the next tagged release.
remuda._api_v5_exit_events = {}
remuda._api_v5_legacy_exit_names = {}
remuda.on("session_exited", function(name, details)
  table.insert(remuda._api_v5_exit_events, { name = name, details = details })
end)
remuda.on("session_exited", function(name)
  table.insert(remuda._api_v5_legacy_exit_names, name)
end)

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

local bad_timeout = pcall(remuda.pending, { timeout = 301 })
assert(not bad_timeout, "pending timeout must not exceed 300 seconds")

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

print("v5 ok")
