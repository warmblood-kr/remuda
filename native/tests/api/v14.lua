-- Like later fixtures, this file becomes frozen once merged.

assert(type(remuda.cluster) == "table", "remuda.cluster namespace")
assert(type(remuda.cluster.nodes) == "function", "cluster.nodes function")
local nodes, reason = remuda.cluster.nodes()
assert(nodes == nil, "the private fixture daemon has no cluster")
assert(reason == "no_cluster", "no_cluster is a distinct reason, not an empty list")

print("v14 ok")
