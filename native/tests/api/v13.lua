-- v13 adds the existing SessionSummary.id to remuda.ls rows.
-- v5 stays frozen; this is an additive Lua projection only.
-- `remuda.new(name)` starts the daemon's portable default shell, so this
-- fixture does not depend on `sh` existing (Windows uses powershell.exe).

local name = "api-v13-id-" .. tostring(os.time())
remuda.new(name)
local row
for _, candidate in ipairs(remuda.ls()) do
  if candidate.name == name then row = candidate end
end
assert(row, "session is listed")
assert(type(row.id) == "string" and #row.id > 0, "remuda.ls row has a stable id")
assert(type(row.instance_id) == "string" and #row.instance_id > 0, "remuda.ls row has instance_id")
assert(row.id ~= row.instance_id, "id and instance_id are distinct")

local closed, close_error = pcall(remuda.close, name)
assert(closed, "fixture session closes: " .. tostring(close_error))

print("v13 ok")
