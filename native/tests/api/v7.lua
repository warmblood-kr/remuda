-- v7 adds strict validation for specs that opt in with version = 2 (or report_version = 2),
-- the "spec" and "unsupported" report kinds, and the strict_v2 capability.
-- Like earlier versions, this file becomes frozen once merged.

local caps = remuda.cli.capabilities()
local function has(list, want)
  for _, item in ipairs(list) do if item == want then return true end end
  return false
end
assert(has(caps.features, "strict_v2") and has(caps.features, "stable_report"), "features")
assert(has(caps.spec_versions, 1) and has(caps.spec_versions, 2), "spec versions")
assert(has(caps.report_versions, 1) and has(caps.report_versions, 2), "report versions")
assert(remuda._registry["cli.capabilities"] ~= nil, "cli.capabilities needs a registry entry")

local function spec()
  return { name = "remuda v7", version = 2, report_version = 2,
    verbs = { go = { next = "remuda v7 go --help", args = { { name = "X", help = "x" } } } } }
end

local ok = remuda.cli.parse(spec(), { "go", "a" })
assert(ok.ok and ok.kind == "success" and ok.values.X == "a", "clean v2 spec parses")

local typo = spec(); typo.vrbs = {}
local r = remuda.cli.parse(typo, { "go", "a" })
assert(not r.ok and r.kind == "spec" and r.code == 2 and r.text:find("vrbs", 1, true), "typo rejected")

local secret = spec(); secret.verbs.go.args[1].name = "SENTINEL SECRET"
r = remuda.cli.parse(secret, { "go", "a" })
assert(r.kind == "spec" and not r.text:find("SENTINEL", 1, true), "values are never echoed")

local future = spec(); future.version = 3
r = remuda.cli.parse(future, { "go", "a" })
assert(r.kind == "unsupported" and r.text:find("Next:", 1, true) and r.text:find("remuda upgrade", 1, true), "upgrade diagnostic")
local need = spec(); need.requires = { "no_such_feature" }
assert(remuda.cli.parse(need, { "go", "a" }).kind == "unsupported", "missing capability")

-- a spec without version keeps the legacy behavior, unknown keys included
local legacy = spec(); legacy.version = nil; legacy.report_version = nil; legacy.vrbs = {}
local l = remuda.cli.parse(legacy, { "go", "a" })
assert(l.ok and l.kind == nil and l.path == nil, "legacy spec unchanged")

-- report_version = 2 alone keeps the envelope-only behavior: unknown keys are ignored
local only = spec(); only.version = nil; only.vrbs = {}
local o = remuda.cli.parse(only, { "go", "a" })
assert(o.ok and o.kind == "success" and type(o.path) == "table", "report_version alone unchanged")

print("v7 ok")
