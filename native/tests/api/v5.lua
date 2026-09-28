-- v5 adds deferred replies for extension commands. Like earlier versions,
-- this file becomes frozen once merged.
assert(type(remuda.pending) == "function", "remuda.pending is missing")
assert(remuda._registry.pending ~= nil, "remuda.pending needs a registry entry")
assert(remuda._pending_replies == nil, "pending manager internals must remain private")

local bad_timeout = pcall(remuda.pending, { timeout = 301 })
assert(not bad_timeout, "pending timeout must not exceed 300 seconds")

print("v5 deferred replies surface ok")
