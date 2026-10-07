-- v9 adds the option field repeat_policy (strict version = 2 specs only) and its capability.
-- Like earlier versions, this file becomes frozen once merged.

local caps = remuda.cli.capabilities()
local found = false
for _, f in ipairs(caps.features) do found = found or f == "repeat_policy" end
assert(found, "repeat_policy capability")

local function spec(opt)
  return { version = 2, name = "remuda v9", requires = { "repeat_policy" },
    verbs = { go = { next = "remuda v9 go --help", options = { opt } } } }
end

local r = remuda.cli.parse(spec({ long = "w", value = "D", help = "h", repeat_policy = "append" }),
  { "go", "--w", "a", "--w=b" })
assert(r.ok and #r.values.w == 2 and r.values.w[2] == "b", "append")
r = remuda.cli.parse(spec({ long = "m", value = "M", help = "h", repeat_policy = "last" }),
  { "go", "--m", "a", "--m", "b" })
assert(r.ok and r.values.m == "b", "last wins")
r = remuda.cli.parse(spec({ long = "j", help = "h", repeat_policy = "coalesce" }), { "go", "--j", "--j" })
assert(r.ok and r.values.j == true, "coalesce")
r = remuda.cli.parse(spec({ long = "m", value = "M", help = "h" }), { "go", "--m", "a", "--m", "b" })
assert(not r.ok and r.kind == "error", "reject is the default")
r = remuda.cli.parse(spec({ long = "j", help = "h", repeat_policy = "append" }), { "go" })
assert(r.kind == "spec" and r.code == 2, "append needs a value")

local v1 = spec({ long = "m", value = "M", help = "h", repeat_policy = "bogus" })
v1.version = nil
assert(remuda.cli.parse(v1, { "go", "--m", "a" }).ok, "v1 ignores repeat_policy")

print("v9 ok")
