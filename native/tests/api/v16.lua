-- v16: after #941 the per-session remote-input opt-out is advisory only.
-- The Lua setter and launch option remain accepted for compatibility, but
-- trusted peers are admitted and the list reports the flag as false.
-- v16 also adds optional bounded team metadata.
local name, instance = remuda.session.new("api-v16")
assert(type(name) == "string" and name ~= "", "session name")
assert(type(instance) == "string" and instance ~= "", "session instance")

local function listed(wanted)
  for _, row in ipairs(remuda.session.list()) do
    if row.name == wanted then return row end
  end
end

local ok, err = remuda.session.remote_input(name, instance, false)
assert(ok == true, tostring(err))
assert(listed(name).remote_input_disabled == false,
  "the opt-out is advisory and must not be reported as enforced")

local wrong, why = remuda.session.remote_input(name, "not-the-instance", false)
assert(wrong == nil and type(why) == "string", "wrong instance is still refused")

local protected_name, protected_instance = remuda.session.new(
  "api-v16-protected", nil, nil, nil, { remote_input_disabled = true })
assert(type(protected_name) == "string" and type(protected_instance) == "string")
assert(listed(protected_name).remote_input_disabled == false,
  "the launch flag is advisory too")

assert(remuda.session.team(protected_name, protected_instance, {
  agent_kind = "codex",
  model = "gpt-6.1",
  depth = 1,
}))
local team = listed(protected_name).team
assert(team and team.agent_kind == "codex", "team agent_kind")
assert(team.model == "gpt-6.1", "team model")
assert(team.depth == 1, "team depth")

local invalid = remuda.session.team(protected_name, protected_instance, { depth = 21 })
assert(invalid == nil, "out-of-range depth is refused")
assert(remuda.session.team(protected_name, protected_instance, nil))
assert(listed(protected_name).team == nil, "cleared team is absent")

remuda.session.close(protected_name)
remuda.session.close(name)
print("v16 ok")
