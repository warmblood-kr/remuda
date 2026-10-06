-- v8 adds remuda.cli.require(need) -> true | false, diagnostic: a pure-Lua gate
-- that checks remuda.cli.capabilities() before a caller sends a v2 spec.
-- Like earlier versions, this file becomes frozen once merged.

assert(remuda._registry["cli.require"] ~= nil, "cli.require needs a registry entry")
local require_caps = remuda.cli.require
local NEXT = "\nNext: remuda upgrade"

assert(require_caps({ features = { "strict_v2", "stable_report" }, spec_version = 2, report_version = 2 }) == true, "satisfied")
assert(require_caps({}) == true, "empty requirement is satisfied")

local ok, text = require_caps({ features = { "strict_v2", "no_such_feature" } })
assert(ok == false and text == "remuda: this core is too old: missing no_such_feature." .. NEXT, text)
ok, text = require_caps({ spec_version = 3, report_version = 9 })
assert(not ok and text == "remuda: this core is too old: missing spec_version 3, report_version 9." .. NEXT, text)
ok, text = require_caps({ spec_version = 2.5 })
assert(not ok and text == "remuda: invalid requirement." .. "\nNext: remuda doc", text)

-- hostile: nothing caller-supplied is echoed, nothing throws
for _, bad in ipairs({ 5, "x", true, { features = "strict_v2" }, { features = { 1 } },
    { features = { {} } }, { spec_version = "2" }, { report_version = {} },
    { features = { "SENTINEL SECRET" } }, { features = { string.rep("a", 200) } } }) do
  local o, t = require_caps(bad)
  assert(o == false and type(t) == "string" and not t:find("SENTINEL", 1, true) and not t:find("aaaa", 1, true), t)
end
assert(select(2, require_caps({ features = { "SENTINEL SECRET" } })) == "remuda: this core is too old: missing <feature>." .. NEXT)
assert(select(2, require_caps(nil)) == "remuda: invalid requirement." .. "\nNext: remuda doc")

-- metatables never run: raw reads only
local hits = 0
local trap = setmetatable({}, { __index = function() hits = hits + 1 end, __len = function() hits = hits + 1 return 1 end })
require_caps(trap)
require_caps({ features = trap })
local meta = setmetatable({ features = { "strict_v2" } }, { __index = function() hits = hits + 1 end })
assert(require_caps(meta) == true and hits == 0, "metamethods ran")

-- old core: capabilities absent (or hostile) means no capabilities
local saved = remuda.cli.capabilities
local function with_caps(value, f)
  remuda.cli.capabilities = value
  local a, b = pcall(f)
  remuda.cli.capabilities = saved
  assert(a, b)
end
with_caps(nil, function()
  local o, t = require_caps({ features = { "strict_v2" } })
  assert(o == false and t == "remuda: this core predates CLI capabilities; upgrade it." .. NEXT, t)
  assert(require_caps({}) == false, "no capabilities even for an empty requirement")
end)
with_caps(function() return { features = { "strict_v2" }, spec_versions = "bad" } end, function()
  assert(require_caps({ spec_version = 2 }) == false and require_caps({ features = { "strict_v2" } }) == true)
end)
with_caps(function() error("boom") end, function()
  local o, t = require_caps({})
  assert(o == false and t == "remuda: this core predates CLI capabilities; upgrade it." .. NEXT, t)
end)

print("v8 ok")
