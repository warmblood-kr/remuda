-- v10 adds the explicit session-level clear_input(name, key) primitive.
-- Like later fixtures, this file becomes frozen once merged.

assert(remuda.clear_input ~= nil, "clear_input needs a top-level binding")
local missing_ok, missing_error = pcall(remuda.clear_input, "missing", "\21")
assert(not missing_ok and tostring(missing_error):find("no such session", 1, true), "missing session raises")
local name = "api-v10-clear-" .. tostring(os.time())
remuda.new(name, { "sh" })
for _, key in ipairs({ "\15", "\27OM", "x", string.rep("x", 17) }) do
  local accepted, err = pcall(remuda.clear_input, name, key)
  assert(not accepted and tostring(err):find("clear key", 1, true), "untrusted clear key must be refused")
end
local result = remuda.clear_input(name, "\21")
assert(type(result) == "table" and result.cleared == nil, "unknown composer text is ok-only")

print("v10 ok")
