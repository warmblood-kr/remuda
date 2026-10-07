-- v10 adds strict-v2 token origins and separator roles.
local caps = remuda.cli.capabilities()
local found = false
for _, f in ipairs(caps.features) do found = found or f == "token_origins" end
assert(found, "token_origins capability")

local spec = { version = 2, report_version = 2,
  requires = { "stable_report", "strict_v2", "repeat_policy", "token_origins" },
  name = "remuda v10", options = {
    { long = "x", short = "x", value = "X", help = "x", global = true },
    { long = "a", short = "a", help = "a", global = true },
    { long = "b", short = "b", help = "b", global = true },
    { long = "c", short = "c", help = "c", global = true },
  },
  verbs = { go = { next = "remuda v10 go --help",
    options = { { long = "m", value = "M", help = "m", repeat_policy = "last" } },
    args = { { name = "ITEM", help = "item", multiple = true } } } },
}

local r = remuda.cli.parse(spec, { "go", "--x=value", "-abc", "--", "-", "repeat", "repeat" })
assert(r.ok, r.text)
assert(r.values.ITEM[1] == "-" and r.values.ITEM[2] == "repeat" and r.values.ITEM[3] == "repeat", "escaped dash and repeated values")
assert(r.values.x == "value" and r.values.a and r.values.b and r.values.c, "inline and cluster parsing")
local escaped = r.origins.ITEM[1]
assert(escaped.argv_index == 5 and escaped.after_separator, "escaped body origin")
assert(r.origins.ITEM[2].argv_index == 6 and r.origins.ITEM[3].argv_index == 7, "equal values retain distinct argv positions")
assert(r.origins.x[1].argv_index == 2 and r.origins.x[1].byte_range[1] == 4 and r.origins.x[1].byte_range[2] == 9, "inline value byte span")
assert(#r.origins.a == 1 and r.origins.a[1].byte_range[1] == 1 and r.origins.a[1].byte_range[2] == 2, "first short cluster occurrence")
assert(#r.origins.b == 1 and r.origins.b[1].byte_range[1] == 2 and r.origins.b[1].byte_range[2] == 3, "second short cluster occurrence")
assert(#r.origins.c == 1 and r.origins.c[1].byte_range[1] == 3 and r.origins.c[1].byte_range[2] == 4, "third short cluster occurrence")
assert(r.boundaries[1].argv_index == 4 and r.boundaries[1].role == "separator", "separator role")

local stdin = remuda.cli.parse(spec, { "go", "-" })
local literal = remuda.cli.parse(spec, { "go", "--", "-" })
assert(stdin.ok and literal.ok, "dash parses in either position")
assert(not stdin.origins.ITEM[1].after_separator and literal.origins.ITEM[1].after_separator, "stdin candidate differs from escaped dash")
assert(stdin.origins.ITEM[1].argv_index == 2 and literal.origins.ITEM[1].argv_index == 3, "dash source positions")

local inline_dash = remuda.cli.parse(spec, { "go", "--x=-", "body" })
assert(inline_dash.ok and inline_dash.values.x == "-", "inline dash value")
assert(inline_dash.origins.x[1].argv_index == 2 and inline_dash.origins.x[1].byte_range[1] == 4 and inline_dash.origins.x[1].byte_range[2] == 5, "inline dash span")

local last = remuda.cli.parse(spec, { "go", "--m", "first", "--m", "second", "body" })
assert(last.ok and last.values.m == "second", "last-value option")
assert(#last.origins.m == 1 and last.origins.m[1].argv_index == 5, "last-value provenance follows retained value")

print("v10 ok")
