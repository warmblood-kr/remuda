-- Local remote-input opt-out: enabled=false disables, enabled=true enables.
local name, instance = remuda.session.new("api-v15", { "sh" })
assert(type(name) == "string" and name ~= "", "session name")
assert(type(instance) == "string" and instance ~= "", "session instance")

local function listed()
  for _, row in ipairs(remuda.session.list()) do
    if row.name == name then return row end
  end
end

local ok, err = remuda.session.remote_input(name, instance, false)
assert(ok == true, tostring(err))
assert(listed().remote_input_disabled == true, "false disables remote input")

ok, err = remuda.session.remote_input(name, instance, true)
assert(ok == true, tostring(err))
assert(listed().remote_input_disabled == false, "true enables remote input")

local wrong, why = remuda.session.remote_input(name, "not-the-instance", false)
assert(wrong == nil and type(why) == "string", "wrong instance is refused")

remuda.session.close(name)
print("v15 ok")
