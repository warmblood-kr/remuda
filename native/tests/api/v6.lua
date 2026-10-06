-- v6 adds remuda.cli.capabilities() and the opt-in report_version = 2 envelope.
-- Like earlier versions, this file becomes frozen once merged.

local caps = remuda.cli.capabilities()
assert(#caps.spec_versions == 1 and caps.spec_versions[1] == 1, "spec versions")
assert(#caps.report_versions == 2 and caps.report_versions[1] == 1
  and caps.report_versions[2] == 2, "report versions")
assert(#caps.features == 1 and caps.features[1] == "stable_report", "features")
assert(remuda._registry["cli.capabilities"] ~= nil, "cli.capabilities needs a registry entry")

local function spec(version)
  return {
    name = "remuda v6", report_version = version,
    options = { { long = "json", help = "j", global = true }, { long = "room", value = "R", help = "r" } },
    verbs = { send = { next = "remuda v6 send --help",
      args = { { name = "TO", help = "to" }, { name = "BODY", help = "b", multiple = true, required = false } } } },
  }
end

-- absent report_version: the v1 report, with no envelope fields
local v1 = remuda.cli.parse(spec(nil), { "send", "a", "x" })
assert(v1.ok and v1.kind == nil and v1.verb == "send" and v1.values.BODY == "x", "v1 report changed")
assert(v1.path == nil and v1.handler == nil and v1.bodies == nil, "v1 must not gain envelope fields")

-- v2: success envelope, vectors for one / many / zero
local one = remuda.cli.parse(spec(2), { "send", "a", "x" })
assert(one.ok and one.kind == "success" and one.code == 0 and one.text == "", "v2 success")
assert(one.verb == "send" and one.handler == "" and one.shape == "", "v2 scalars")
assert(type(one.path) == "table" and type(one.origins) == "table"
  and type(one.boundaries) == "table" and type(one.bodies) == "table", "v2 tables")
assert(#one.values.BODY == 1 and one.values.BODY[1] == "x", "one value is an array")
local many = remuda.cli.parse(spec(2), { "send", "a", "x", "y" })
assert(#many.values.BODY == 2 and many.values.BODY[2] == "y", "many values are an array")
local zero = remuda.cli.parse(spec(2), { "send", "a" })
assert(type(zero.values.BODY) == "table" and #zero.values.BODY == 0, "zero values are an empty array")
assert(zero.values.room == nil, "absent optional scalar stays absent")
assert(zero.values.json == false, "absent flag is false, not absent")

-- v2: help and error clear values
local help = remuda.cli.parse(spec(2), { "send", "--help" })
assert(not help.ok and help.kind == "help" and help.code == 0 and next(help.values) == nil, "v2 help")
local err = remuda.cli.parse(spec(2), { "send", "--nope" })
assert(not err.ok and err.kind == "error" and err.code == 2 and next(err.values) == nil, "v2 error")
local unknown = remuda.cli.parse(spec(2), { "zzz" })
assert(not unknown.ok and unknown.kind == "error" and #unknown.path == 0, "v2 unknown verb")

print("v6 ok")
