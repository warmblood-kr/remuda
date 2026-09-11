-- The tool vocabulary: the frame is Rust, the words are Lua.
--
-- 정수님, 2026-09-10: *"MCP server는 제공을 하고, 필요에 따라서 tool을 추가해나갈 수
-- 있도록."* A tool here is an ordinary Lua function that has been marked
-- exported. `mcp.rs` reflects this table into `tools/list` and dispatches
-- `tools/call` back into it, so a new tool costs no rebuild and no redeploy.
--
-- Written in Lua rather than Rust on purpose. Building the frame's own
-- vocabulary in the host would be doing there the job the guest was embedded to
-- do — the same argument that put the polling helper of `steps/005` in Lua.
--
-- A word is callable AND carries its own description, so the next tool can be
-- written out of the last one: `remuda.tools.wait_for{session = s, pattern = p}`
-- is a normal call from any script, `-e`, or REPL line. That is what makes this
-- a vocabulary that accumulates rather than a side table of closures.

remuda.tools = {}

-- Every word answers a call, so a tool defined today is a primitive tomorrow.
local speech = {
  __call = function(word, arguments) return word.run(arguments or {}) end,
  __tostring = function(word) return "tool " .. word.name end,
}

-- Define a word and export it as an MCP tool. `args` maps each argument to a
-- description a model reads; `needs` lists the ones that are not optional.
-- Redefining an existing word replaces it — 정수님, 2026-09-10: 내부에서
-- 스스로를 변경할 수 있어도 돼요.
function remuda.tool(spec)
  local name = spec.name
  if type(name) ~= "string" or name == "" then
    error("a tool needs a name", 2)
  end
  -- A description is what a model chooses from, so an unusable one is refused
  -- here rather than served and never called.
  if type(spec.about) ~= "string" or #spec.about < 20 then
    error("tool " .. name .. " needs an `about` long enough to choose from", 2)
  end
  if type(spec.run) ~= "function" then
    error("tool " .. name .. " needs a `run` function", 2)
  end
  local args = spec.args or {}
  local needs = spec.needs or {}
  for _, key in ipairs(needs) do
    if args[key] == nil then
      error("tool " .. name .. " needs `" .. key .. "` but never describes it", 2)
    end
  end
  local word = setmetatable({
    name = name,
    about = spec.about,
    args = args,
    needs = needs,
    run = spec.run,
  }, speech)
  remuda.tools[name] = word
  return word
end

local escapes = {
  ['"'] = '\\"',
  ["\\"] = "\\\\",
  ["\n"] = "\\n",
  ["\r"] = "\\r",
  ["\t"] = "\\t",
}

-- One JSON string. Non-ASCII bytes travel as they are: a Lua string is bytes,
-- the transport is UTF-8, and re-encoding them would only invent a second bug.
local function quoted(text)
  local escaped = tostring(text):gsub('[%c"\\]', function(c)
    return escapes[c] or string.format("\\u%04X", c:byte())
  end)
  return '"' .. escaped .. '"'
end

local function sorted_keys(table_value)
  local keys = {}
  for key in pairs(table_value) do
    keys[#keys + 1] = key
  end
  -- Lua's hash order differs between runs, and `tools/list` is something a
  -- person diffs. Sorted, so the same registry always renders the same list.
  table.sort(keys)
  return keys
end

-- What `tools/list` adds to the frame's own five, as MCP descriptor JSON.
-- Rust asks for this by name; keep the shape or `mcp.rs` will not parse it.
function remuda._descriptors()
  local out = {}
  for _, name in ipairs(sorted_keys(remuda.tools)) do
    local word = remuda.tools[name]
    local properties = {}
    for _, key in ipairs(sorted_keys(word.args)) do
      properties[#properties + 1] = quoted(key)
        .. ':{"type":"string","description":'
        .. quoted(word.args[key])
        .. "}"
    end
    local required = {}
    for _, key in ipairs(word.needs) do
      required[#required + 1] = quoted(key)
    end
    out[#out + 1] = '{"name":'
      .. quoted(name)
      .. ',"description":'
      .. quoted(word.about)
      .. ',"inputSchema":{"type":"object","properties":{'
      .. table.concat(properties, ",")
      .. '},"required":['
      .. table.concat(required, ",")
      .. "]}}"
  end
  return "[" .. table.concat(out, ",") .. "]"
end

-- One `tools/call`. A missing tool and a missing argument both raise, because
-- the daemon turns a raise into `isError: true` and a return into success — and
-- a model reads a successful empty answer as an answer.
function remuda._call(name, arguments)
  local word = remuda.tools[name]
  if not word then
    error("no such tool: " .. tostring(name), 0)
  end
  arguments = arguments or {}
  for _, key in ipairs(word.needs) do
    if arguments[key] == nil then
      error(name .. " needs `" .. key .. "`", 0)
    end
  end
  local answer = word(arguments)
  if answer == nil then
    return ""
  end
  return tostring(answer)
end

-- Type TEXT into SESSION and submit it with Return, as one act `remuda.feed`
-- will not let a second sender split. Not an MCP tool — a plain stdlib
-- function beside `send`/`insert`, since remuda itself frames none of this
-- (no default pause, no paste sequence) and this is the caller that does.
-- SETTLE (seconds before the submitting Return) defaults to 0.1. Like
-- `remuda.feed`, this blocks the calling Image for SETTLE seconds.
function remuda.type_text(session, text, settle)
  local body = tostring(text):gsub("\r\n?", "\n"):gsub("\27", "")
  settle = settle or 0.1
  local typed = body:find("\n", 1, true) and ("\27[200~" .. body .. "\27[201~") or body
  remuda.feed(session, {
    { burst = typed },
    { pause = settle },
    { burst = "\r" },
  })
end

-- The first word, and the one `steps/008` found missing: readiness. Driving an
-- agent means waiting for it, and every caller so far has written this loop
-- again — `tests/api/v1.lua` has its own copy.
remuda.tool({
  name = "wait_for",
  about = "Wait until a session's screen matches a Lua pattern, then answer with "
    .. "that screen. Fails when the deadline passes instead of answering with a "
    .. "screen that does not match. Use it after `send` rather than guessing a sleep.",
  args = {
    session = "The session to watch.",
    pattern = "A Lua pattern the screen must match. `%$ %s*$` is a shell prompt.",
    seconds = "How long to wait before giving up. Default 30.",
  },
  needs = { "session", "pattern" },
  run = function(a)
    -- MCP argument values arrive as strings; every schema this frame emits says
    -- so. A number is what this one means, and `tonumber` is where that is said.
    local seconds = tonumber(a.seconds) or 30
    local screen
    for _ = 1, math.max(1, math.ceil(seconds / 0.1)) do
      screen = remuda.capture(a.session)
      if screen:find(a.pattern) then
        return screen
      end
      remuda.sleep(0.1)
    end
    error(
      string.format(
        "%s never matched %q within %gs. last screen:\n%s",
        a.session,
        a.pattern,
        seconds,
        screen or ""
      ),
      0
    )
  end,
})
