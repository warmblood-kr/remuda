-- Local remote-input opt-out: enabled=false disables, enabled=true enables.
local name, instance = remuda.session.new("api-v15")
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

-- The launch option must protect the instance before the first listing.
local protected_name, protected_instance = remuda.session.new(
  "api-v15-protected", nil, nil, nil, { remote_input_disabled = true })
assert(type(protected_name) == "string" and type(protected_instance) == "string")
local protected_row
for _, row in ipairs(remuda.session.list()) do
  if row.name == protected_name then protected_row = row end
end
assert(protected_row and protected_row.remote_input_disabled == true,
  "launch-time protection is visible immediately")

local bad_type = pcall(remuda.session.new, "api-v15-bad", nil, nil, nil,
  { remote_input_disabled = "yes" })
assert(bad_type == false, "a non-boolean protection option is refused")
local bad_key = pcall(remuda.session.new, "api-v15-unknown", nil, nil, nil,
  { protected = true })
assert(bad_key == false, "an unknown option key is refused")

remuda.session.close(protected_name)
remuda.session.close(name)
print("v15 ok")
