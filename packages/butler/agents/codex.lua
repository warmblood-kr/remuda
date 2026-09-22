local builders = assert(remuda._butler_agent_builders)
local support = assert(remuda._butler_agent_support)
local telemetry = assert(remuda._butler_telemetry_adapters)

local function json_string(value)
  return '"' .. tostring(value):gsub('\\', '\\\\'):gsub('"', '\\"'):gsub('\n', '\\n'):gsub('\r', '\\r') .. '"'
end

local function app_server_spec(spec)
  local model = spec.model and spec.model ~= "" and json_string(spec.model) or "null"
  local instructions = json_string(spec.system_prompt
    or "This session is managed by Remuda Butler. The remuda butler CLI is available for coordination.")
  return '{"program":["codex","app-server","--stdio"],"start":{"method":"thread/start","params":{'
    .. '"cwd":null,"ephemeral":true,"config":' .. support.mcp_config(spec.token)
    .. ',"approvalsReviewer":"auto_review","model":' .. model
    .. ',"developerInstructions":' .. instructions
    .. '}},"thread_id":"/thread/id","model":"/thread/model",'
    .. '"turn":{"method":"turn/start","params":{"threadId":"$thread_id",'
    .. '"input":[{"type":"text","text":"$input"}]}},'
    .. '"turn_complete":"turn/completed","output_text":"/params/delta",'
    .. '"telemetry":{"method":"thread/tokenUsage/updated",'
    .. '"window":"/params/tokenUsage/modelContextWindow",'
    .. '"used":["/params/tokenUsage/last/inputTokens",'
    .. '"/params/tokenUsage/last/cachedInputTokens",'
    .. '"/params/tokenUsage/last/cacheWriteInputTokens"]}}'
end

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
  return { "remuda", "_json_rpc_terminal", "--status", spec.telemetry.status_path,
    "--spec", app_server_spec(spec) }
end
