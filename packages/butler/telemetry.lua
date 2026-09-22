remuda._butler_telemetry_adapters = remuda._butler_telemetry_adapters or {}

function remuda._butler_telemetry_for(agent)
  local adapter = remuda._butler_telemetry_adapters[agent.kind]
  local telemetry = adapter and adapter.read and adapter.read(agent.telemetry or {}) or {}
  return {
    model = telemetry.model or agent.model or "?",
    context_used = telemetry.context_used or "?",
    context_window = telemetry.context_window or "?",
    context_percent = telemetry.context_percent or "?",
  }
end
