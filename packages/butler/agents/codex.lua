local builders = assert(remuda._butler_agent_builders)
local support = assert(remuda._butler_agent_support)
local telemetry = assert(remuda._butler_telemetry_adapters)

telemetry.codex = {
  setup = function(spec)
    return { model = spec.model, status_path = os.tmpname() .. "." .. spec.name .. ".status" }
  end,
  read = function(state)
    local status = state.status_path and io.open(state.status_path, "r")
    if not status then return { model = state.model } end
    local line = status:read("*l")
    status:close()
    if not line then return { model = state.model } end
    local model, used, window, percent = line:match(
      "^MODEL:([A-Za-z0-9_.%-?]+) CTX:([0-9?]+) CTXWIN:([0-9?]+) CTXPCT:([0-9?]+)$"
    )
    return { model = model or state.model, context_used = used, context_window = window, context_percent = percent }
  end,
}

builders.codex = function(spec)
  local argv = { "remuda", "_codex_app_server", "--status", spec.telemetry.status_path,
    "--mcp-config", support.mcp_config(spec.token), "--instructions", spec.system_prompt
      or "This session is managed by Remuda Butler. The remuda butler CLI is available for coordination." }
  if spec.model and spec.model ~= "" then argv[#argv + 1] = "--model"; argv[#argv + 1] = spec.model end
  return argv
end
