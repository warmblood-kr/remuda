-- v12 adds local exact-instance publication of optional public display facts.
-- Earlier API fixtures remain unchanged.
assert(type(remuda.session.set_display) == "function", "session.set_display")
assert(remuda._registry["session.set_display"] ~= nil, "display vocabulary entry")
local ok = pcall(remuda.session.set_display, "api-v12-missing", "", {
  version = 1, provider = "butler", kind = "codex",
})
assert(not ok, "publication must reject a missing exact session instance")
print("v12 ok")
