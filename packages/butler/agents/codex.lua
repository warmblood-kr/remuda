local builders = assert(remuda._butler_agent_builders)
local telemetry = assert(remuda._butler_telemetry_adapters)

telemetry.codex = {
  setup = function(spec)
    return { model = spec.model, status_path = os.tmpname() .. "." .. spec.name .. ".status" }
  end,
  read = function(state)
    local f = state.status_path and io.open(state.status_path, "r")
    if not f then return { model = state.model } end
    local line = f:read("*l")
    f:close()
    local model, used, window, percent = (line or ""):match("^MODEL:([A-Za-z0-9_.%-?]+) CTX:([0-9?]+) CTXWIN:([0-9?]+) CTXPCT:([0-9?]+)$")
    return { model = model or state.model, context_used = used, context_window = window, context_percent = percent }
  end,
}

builders.codex = function(spec)
  return { "remuda", "_codex_tui", "--status", spec.telemetry.status_path }
end
