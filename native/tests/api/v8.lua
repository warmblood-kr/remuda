-- v8 adds remuda.cli.require(need) -> true | false, diagnostic: a pure-Lua gate
-- that checks remuda.cli.capabilities() before a caller sends a v2 spec.
-- Like earlier versions, this file becomes frozen once merged.

assert(remuda._registry["cli.require"] ~= nil, "cli.require needs a registry entry")
local require_caps = remuda.cli.require
local NEXT = "\nNext: remuda upgrade"

-- No assertion here depends on the real core's feature set: a fake capability
-- table is installed for the whole run and restored afterwards.
local real_caps = remuda.cli.capabilities
local function fake(features)
  return function() return { spec_versions = { 1, 2 }, report_versions = { 1, 2 }, features = features } end
end
remuda.cli.capabilities = fake({ "strict_v2", "stable_report" })
local function with_caps(value, f)
  local prev = remuda.cli.capabilities
  remuda.cli.capabilities = value
  local a, b = pcall(f)
  remuda.cli.capabilities = prev
  assert(a, b)
end
local function main()
  assert(require_caps({ features = { "strict_v2", "stable_report" }, spec_version = 2, report_version = 2 }) == true, "satisfied")
  assert(require_caps({}) == true, "empty requirement is satisfied")

  local ok, text = require_caps({ features = { "strict_v2", "no_such_feature" } })
  assert(ok == false and text == "remuda: this core is too old: missing <feature>." .. NEXT, text)
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

  -- fail closed: oversized, sparse or named-key lists are malformed, never truncated
  local BAD = "remuda: invalid requirement.\nNext: remuda doc"
  local big = {}
  for i = 1, 1025 do big[i] = "strict_v2" end
  big[1025] = "missing_future_capability"
  assert(select(2, require_caps({ features = big })) == BAD, "1025 entries rejected")
  assert(select(2, require_caps({ features = { [4096] = "missing_future_capability" } })) == BAD, "sparse")
  assert(select(2, require_caps({ features = { missing_future_capability = true } })) == BAD, "named key")
  assert(select(2, require_caps({ features = { "strict_v2", x = "strict_v2" } })) == BAD, "mixed key")
  local full = {}
  for i = 1, 1024 do full[i] = "strict_v2" end
  assert(require_caps({ features = full }) == true, "1024 satisfied")
  full[1024] = "missing_future_capability"
  assert(select(2, require_caps({ features = full })) == "remuda: this core is too old: missing <feature>." .. NEXT, "1024 with a missing one")

  -- caller strings are never echoed; only the reviewed public names are
  assert(select(2, require_caps({ features = { "secret_api_key_123" } })) == "remuda: this core is too old: missing <feature>." .. NEXT)
  assert(select(2, require_caps({ features = { "repeat_policy" } })) == "remuda: this core is too old: missing repeat_policy." .. NEXT)
  local allowlisted_ok
  with_caps(fake({ "repeat_policy" }), function() allowlisted_ok = require_caps({ features = { "repeat_policy" } }) end)
  assert(allowlisted_ok == true, "a core advertising the feature satisfies it")

  -- string work is bounded: 1024 references to one 1 MiB string
  local huge = string.rep("a", 1 << 20)
  local refs = {}
  for i = 1, 1024 do refs[i] = huge end
  local t0 = os.clock()
  assert(select(2, require_caps({ features = refs })):find("<feature>", 1, true))
  assert(os.clock() - t0 < 1, "huge feature strings must not be scanned")

  -- metatables never run: raw reads only
  local hits = 0
  local trap = setmetatable({}, { __index = function() hits = hits + 1 end, __len = function() hits = hits + 1 return 1 end })
  require_caps(trap)
  require_caps({ features = trap })
  local meta = setmetatable({ features = { "strict_v2" } }, { __index = function() hits = hits + 1 end })
  assert(require_caps(meta) == true and hits == 0, "metamethods ran")

  -- old core: capabilities absent (or hostile) means no capabilities
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

end
local good, err = pcall(main)
remuda.cli.capabilities = real_caps
assert(good, err)

print("v8 ok")
