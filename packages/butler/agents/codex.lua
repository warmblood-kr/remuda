local builders = assert(remuda._butler_agent_builders)
local support = assert(remuda._butler_agent_support)
local telemetry = assert(remuda._butler_telemetry_adapters)

telemetry.codex = {
  setup = function(spec)
    return { model = spec.model }
  end,
  read = function(state)
    return { model = state.model }
  end,
}

builders.codex = function(spec)
  local argv = { "codex" }
  for _, flag in ipairs(support.mcp_flags(spec.token)) do argv[#argv + 1] = flag end
  argv[#argv + 1] = "--approve-for-me"
  if spec.model and spec.model ~= "" then argv[#argv + 1] = "--model"; argv[#argv + 1] = spec.model end
  return argv
end
