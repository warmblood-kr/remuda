-- v5 adds the session namespace and deferred replies for extension commands.
-- v1-v4 remain frozen; v5 remains open until the next tagged release.
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

print("v5 ok")
