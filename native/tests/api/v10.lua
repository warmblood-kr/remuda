-- v10 adds the explicit session-level clear_input(name, key) primitive.
-- Like later fixtures, this file becomes frozen once merged.

assert(remuda.clear_input ~= nil, "clear_input needs a top-level binding")
local missing_ok, missing_error = pcall(remuda.clear_input, "missing", "\21")
assert(not missing_ok and tostring(missing_error):find("no such session", 1, true), "missing session raises")
local name = "api-v10-clear-" .. tostring(os.time())
remuda.new(name, { "sh" })
local result = remuda.clear_input(name, "\21")
assert(type(result) == "table" and result.cleared == nil, "unknown composer text is ok-only")

print("v10 ok")
