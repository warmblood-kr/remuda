-- Like later fixtures, this file becomes frozen once merged.

assert(type(remuda.daemon) == "table", "remuda.daemon namespace")
local result = remuda.daemon.socket_publication()
assert(result.state == "staged" or result.state == "legacy", "state is staged or legacy")
assert((result.state == "legacy") == (type(result.reason) == "string"), "only legacy carries a reason")

print("v11 ok")
