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

-- One row per word, Rust's own bindings included (`script.rs`'s `WORDS`
-- populates this table before this file loads) — `remuda doc` reads it.
local function register(name, about, signature)
  remuda._registry[name] = { name = name, about = about, signature = signature }
end
register("tools", "The `remuda.tool` registry table, keyed by tool name.", "table")

-- Required names first (a caller's own order, via `needs`), then everything
-- else marked optional — the same order a hand-written signature would use.
local function arg_list(args, needs)
  local seen, parts = {}, {}
  for _, key in ipairs(needs) do
    parts[#parts + 1] = key
    seen[key] = true
  end
  local rest = {}
  for key in pairs(args) do
    if not seen[key] then rest[#rest + 1] = key end
  end
  table.sort(rest)
  for _, key in ipairs(rest) do
    parts[#parts + 1] = key .. "?"
  end
  return table.concat(parts, ", ")
end

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
  register(name, spec.about, name .. "(" .. arg_list(args, needs) .. ") -> string")
  return word
end
register("tool", "Define a word and export it as an MCP tool.", "tool(spec) -> word")

-- Keyed by HANDLE, not name — two schedules may share a label, or carry
-- none. A Lua table is a legal table key, and the handle already is one, so
-- there is no separate id to keep in sync with it.
remuda.schedules = {}
register("schedules", "The `remuda.schedule` registry table, keyed by the handle `schedule()` returned.", "table")

-- Keyed by NAME, unlike `remuda.schedules` above — an unnamed schedule has no
-- key to count under, so `_run_due_schedules` skips it rather than index a
-- table with nil.
remuda._schedule_fire_counts = {}
register(
  "_schedule_fire_counts",
  "Internal named-schedule fire counts. Read via `schedule_fires()`.",
  "table"
)

local Schedule = {}
Schedule.__index = Schedule

-- 정수님, 2026-09-12: "다른 익스텐션들도 자기 스케쥴들을 등록할 수 있어야 합니다"
-- — multi-registrant from the first line, the same `remuda.tool` shape. Native
-- knows only ITS OWN fixed tick period; EVERY seconds and EVERY callback are
-- this table's business alone, matching remuda.tool's split (Rust reflects,
-- Lua decides). Does not survive a daemon restart — same ceiling as
-- `remuda.tools` (`steps/014-a-tool-registry.md:292-295`), not solved here.
--
-- NAME is an optional label, never an identity — two extensions (or one,
-- twice) may register under the same name without one silently replacing
-- the other, the gap measured on 09-13. Ownership is the returned handle;
-- only `remuda.cancel(handle)` removes it.
function remuda.schedule(spec)
  if spec.name ~= nil and (type(spec.name) ~= "string" or spec.name == "") then
    error("a schedule's name, when given, must be a non-empty string", 2)
  end
  if type(spec.every) ~= "number" or spec.every <= 0 then
    error("a schedule needs a positive `every` (seconds)", 2)
  end
  if type(spec.run) ~= "function" then
    error("a schedule needs a `run` function", 2)
  end
  local handle = setmetatable({ name = spec.name }, Schedule)
  remuda.schedules[handle] = {
    name = spec.name,
    every = spec.every,
    run = spec.run,
    last_run = 0,
  }
  return handle
end
register("schedule", "Register a periodic callback, run every `every` seconds.", "schedule(spec) -> handle")

-- A no-op on an already-cancelled or unrecognized handle — a caller racing
-- its own cancel, or cancelling twice, gets silence rather than an error for
-- something that already happened.
function remuda.cancel(handle)
  remuda.schedules[handle] = nil
end
register("cancel", "Cancel a schedule by the handle `schedule()` returned.", "cancel(handle) -> nil")

-- Called once per native tick with the current time (seconds, native's
-- clock). Fires every schedule whose own interval has elapsed since ITS OWN
-- last run — native never sees or compares an individual interval itself.
function remuda._run_due_schedules(now)
  for _, schedule in pairs(remuda.schedules) do
    if now - schedule.last_run >= schedule.every then
      schedule.last_run = now
      if schedule.name then
        remuda._schedule_fire_counts[schedule.name] = (remuda._schedule_fire_counts[schedule.name] or 0) + 1
      end
      schedule.run()
    end
  end
end
register(
  "_run_due_schedules",
  "Fire every schedule whose interval has elapsed. Called once per native tick.",
  "_run_due_schedules(now) -> nil"
)

-- A shallow copy, the same discipline `emit`'s own hook snapshot already
-- keeps — a caller mutating what it was handed must never reach back into
-- this table.
function remuda.schedule_fires()
  local copy = {}
  for k, v in pairs(remuda._schedule_fire_counts) do
    copy[k] = v
  end
  return copy
end
register("schedule_fires", "How many times each named schedule has fired.", "schedule_fires() -> {[name]=n}")

-- hooks: Emacs's augroup model. `on` files a callback under an event name;
-- `group` is optional on registration but required to clear by, the same
-- asymmetry augroup has — naming a group costs nothing, but clearing without
-- one would wipe every extension's hooks at once, not just the caller's own.
remuda.hooks = {}
register("hooks", "The `remuda.on` registry table, keyed by event name.", "table")

-- Keyed by event name, counting every `emit` call for it regardless of
-- whether a hook is registered — `remuda.hooks` above only knows the events
-- someone `on`'d, not the ones only ever `emit`'d.
remuda._event_counts = {}
register("_event_counts", "Internal event-emit counts, keyed by event name. Read via `event_counts()`.", "table")

function remuda.on(event, fn, opts)
  if type(event) ~= "string" or event == "" then
    error("a hook needs an event name", 2)
  end
  if type(fn) ~= "function" then
    error("a hook needs a function", 2)
  end
  opts = opts or {}
  remuda.hooks[event] = remuda.hooks[event] or {}
  table.insert(remuda.hooks[event], { fn = fn, group = opts.group })
end
register("on", "Register a callback to run when an event fires.", "on(event, fn, opts?) -> nil")

-- A snapshot, not a live reference to `remuda.hooks[event]` — a hook that
-- calls `clear_hooks` on its own group must not skip or re-run a sibling
-- still mid-iteration.
function remuda.emit(event, ...)
  remuda._event_counts[event] = (remuda._event_counts[event] or 0) + 1
  local hooks = remuda.hooks[event]
  if not hooks then
    return
  end
  local snapshot = {}
  for i, hook in ipairs(hooks) do
    snapshot[i] = hook
  end
  for _, hook in ipairs(snapshot) do
    hook.fn(...)
  end
end
register("emit", "Fire an event, running every hook registered for it.", "emit(event, ...) -> nil")

-- A shallow copy, the same discipline `emit` itself already keeps for its own
-- hook snapshot above — a caller mutating what it was handed must never
-- reach back into this table.
function remuda.event_counts()
  local copy = {}
  for k, v in pairs(remuda._event_counts) do
    copy[k] = v
  end
  return copy
end
register("event_counts", "How many times each event has been emitted.", "event_counts() -> {[event]=n}")

-- Every hook in GROUP, across every event — an augroup clears as a unit
-- regardless of which events its members are on, so one extension's cleanup
-- can never reach a hook another extension (or group) registered.
function remuda.clear_hooks(opts)
  opts = opts or {}
  if opts.group == nil then
    error("clear_hooks needs a `group` — clearing every hook at once is not offered", 2)
  end
  for event, hooks in pairs(remuda.hooks) do
    local kept = {}
    for _, hook in ipairs(hooks) do
      if hook.group ~= opts.group then
        kept[#kept + 1] = hook
      end
    end
    remuda.hooks[event] = kept
  end
end
register("clear_hooks", "Remove every hook registered under a group.", "clear_hooks(opts) -> nil")

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

-- buffer and window: named text an extension can create and show, and a
-- screen rectangle to show it in — Emacs's own split. Pure Lua state, the
-- same shape as `remuda.tools`/`remuda.schedules` above: it does not survive
-- a daemon restart, and that is not a gap to close — a buffer's whole life
-- is the daemon's (see the module doc; nothing here reaches for `Registry`).
-- remuda.buffer knows nothing about what its text MEANS — `Session`'s
-- alive/attached facts, if a buffer's content is built from them, are
-- read by the caller and handed over as plain text, never by this table.

remuda.buffers = {}
register("buffers", "The `remuda.buffer` registry table, keyed by buffer name.", "table")

local Buffer = {}
Buffer.__index = Buffer

remuda.buffer = {}
register("buffer", "Namespace for creating and listing named text buffers.", "table")

-- Create-if-absent, return-if-present — so two extensions naming the same
-- buffer share it rather than racing to overwrite it.
function remuda.buffer.new(name)
  if type(name) ~= "string" or name == "" then
    error("a buffer needs a name", 2)
  end
  local existing = remuda.buffers[name]
  if existing then
    return existing
  end
  local b = setmetatable({ name = name, text = "" }, Buffer)
  remuda.buffers[name] = b
  return b
end

function remuda.buffer.list()
  return sorted_keys(remuda.buffers)
end

-- The free-function spelling of `buffer.new(name):set(text)`, for a caller
-- that has a name but never needed the handle itself.
function remuda.buffer.set(name, text)
  remuda.buffer.new(name):set(text)
end

function Buffer:set(text)
  self.text = tostring(text)
end

function Buffer:append(line)
  self.text = self.text .. tostring(line)
end

function Buffer:get()
  return self.text
end

-- session ≠ buffer: a session is a live process this daemon runs, a buffer
-- is Lua-owned text. `session.buffer` is the buffer named after the session
-- (create-if-absent, same as `buffer.new`) — a convenience, never the session
-- itself, so nothing here duplicates what `remuda.ls()` already reports.
local Session = {}
Session.__index = function(self, key)
  if key == "buffer" then
    return remuda.buffer.new(self.name)
  elseif key == "is_busy" then
    for _, row in ipairs(remuda.ls()) do
      if row.name == self.name then
        -- A session that just produced output is doing work; one that has
        -- sat quiet a couple of seconds is waiting on something else. No
        -- real "is this session working" signal exists — this is a heuristic
        -- ceiling on top of the idle time `ls()` already tracks, not a fact.
        return row.idle < 2.0
      end
    end
    return nil
  end
  return rawget(Session, key)
end

-- A handle onto an existing session, by name. `is_busy`/`context_left` live
-- HERE, never on `remuda.buffer` — a buffer is inert text and has no notion
-- of busy or of a context budget, whichever session's content it happens to
-- hold. (`context_left` is not implemented: a generic pty has no channel a
-- caller's token budget would arrive on. Named so a future one lands here.)
function remuda.session(name)
  if type(name) ~= "string" or name == "" then
    error("a session needs a name", 2)
  end
  return setmetatable({ name = name }, Session)
end
register("session", "A handle onto an existing session, by name.", "session(name) -> session")

-- window: a screen rectangle showing exactly one buffer or attached session,
-- owning the lifetime of neither — closing one kills nothing it showed
-- (`steps/006-lifetime.md`'s tmux rejection stays in force; only the naming
-- that shell "windows" own a session's life is what changed). No layout tree
-- yet — `split` makes a second, independent window, not a nested pane; a
-- tree is a later question if one ever actually shows up.

remuda.windows = {}
register("windows", "The `remuda.window` registry table, keyed by window id.", "table")
local next_window_id = 0

local Window = {}
Window.__index = Window

remuda.window = {}
register("window", "Namespace for the current screen window.", "table")

-- The one window that exists before anything ever splits: today's whole
-- screen, in the vocabulary this module adds. Nothing renders through it
-- yet (`native/src/tui.rs` still owns the real screen) — it exists so a
-- script can hold a handle without special-casing "there's no window yet".
function remuda.window.current()
  local w = remuda.windows["main"]
  if not w then
    w = setmetatable({ id = "main", shows = nil }, Window)
    remuda.windows["main"] = w
  end
  return w
end

-- SIDE is "right" or "below", per the design's own minimal surface — kept as
-- given rather than validated against a fixed list, since nothing downstream
-- interprets it yet (no real geometry exists until a later round wires this
-- into the terminal).
function Window:split(side)
  next_window_id = next_window_id + 1
  local w = setmetatable({ id = "w" .. next_window_id, shows = nil, side = side }, Window)
  remuda.windows[w.id] = w
  return w
end

-- TARGET is a buffer (from `remuda.buffer.new`) or a session name string —
-- this window's business is only "what is showing here", never the target's
-- lifetime.
function Window:show(target)
  self.shows = target
end

function Window:close()
  remuda.windows[self.id] = nil
end

-- What `window_shown_session` (tui.rs) calls instead of poking `.shows`
-- directly: leaves an explicitly shown buffer alone unless SELECTION_CHANGED
-- says the user just moved the selection — a bare tick must never reclaim
-- the window. Wire-internal like `_refresh_sessions_buffer`, not a script's
-- tool, but still registered below: every name on `remuda` is, so the
-- surface test (`the_bound_surface_is_exactly_the_protocols`) stays exact.
function remuda._sync_window_shown(name, selection_changed)
  local w = remuda.window.current()
  if getmetatable(w.shows) == Buffer and not selection_changed then
    return "buffer:" .. w.shows.name
  end
  w.shows = name
  if name == nil then
    return "nil"
  end
  return "session:" .. name
end
register(
  "_sync_window_shown",
  "Reconcile the current window with the session Rust wants to auto-follow.",
  "_sync_window_shown(name, selection_changed) -> string"
)

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
register(
  "_descriptors",
  "MCP tool descriptors for everything `remuda.tool` has registered.",
  "_descriptors() -> string"
)

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
register("_call", "Dispatch one MCP tools/call by name.", "_call(name, arguments) -> string")

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
register(
  "type_text",
  "Type text into a session and submit it with Return.",
  "type_text(session, text, settle?) -> nil"
)

-- The left session list, re-expressed as the "*sessions*" buffer instead of
-- being drawn straight out of Rust.
--
-- `tui.rs`'s `list_row` keeps exactly two things it always had: the cursor
-- mark (which row is selected is a per-viewer fact, not buffer content —
-- the same reason an Emacs buffer does not store which window's point is
-- where) and the width-aware padding/truncation math (mechanism, the same
-- boundary `capture`'s styled-vs-plain split already draws — see
-- `script.rs`'s doc comment on why color stays in the frame). Everything
-- this function decides — the tail word, the empty-herd copy — is content,
-- and content is what a buffer holds.
--
-- WIDTH is passed in rather than read from anywhere, because whether the
-- tail shows `live`/`dead` or just the flag depends on the caller's own
-- column budget — a fact only the renderer asking for a refresh has.
function remuda._refresh_sessions_buffer(width)
  local sessions = remuda.ls()
  local lines = {}
  if #sessions == 0 then
    lines = { "the herd is empty.", "press n to start a session." }
  else
    for i, s in ipairs(sessions) do
      local flag = s.attached and "⚑" or " "
      local state = s.alive and "live" or "dead"
      lines[i] = (width >= 22) and (state .. " " .. flag) or flag
    end
  end
  remuda.buffer.new("*sessions*"):set(table.concat(lines, "\n"))
end
register(
  "_refresh_sessions_buffer",
  "Rebuild the *sessions* buffer's content.",
  "_refresh_sessions_buffer(width) -> nil"
)

-- Sorted "name(signature) -- about", one per line — the manual `remuda doc`
-- prints. Lives beside `_descriptors` since both walk a registry to render it.
function remuda._registry_dump()
  local lines = {}
  for _, name in ipairs(sorted_keys(remuda._registry)) do
    local word = remuda._registry[name]
    lines[#lines + 1] = word.signature .. " -- " .. word.about
  end
  return table.concat(lines, "\n")
end
register("_registry_dump", "Render the word registry as a sorted, human-readable manual.", "_registry_dump() -> string")

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

-- request_counts/schedule_skips (steps/033's follow-up) were readable from
-- Lua but not MCP: remuda._call only reaches remuda.tools, a separate
-- registry from the top-level remuda table these two already live on.
-- Registering them here is the fix — the same remuda.tool every other MCP
-- tool already uses, not a new mechanism.
remuda.tool({
  name = "request_counts",
  about = "The daemon's own request-dispatch counts (list/eval/capture_styled), "
    .. "counted at the one place every round trip crosses. Use it to measure "
    .. "how many round trips a real operation actually costs.",
  run = function()
    local c = remuda.request_counts()
    return "list=" .. c.list .. ",eval=" .. c.eval .. ",capture_styled=" .. c.capture_styled
  end,
})

remuda.tool({
  name = "schedule_skips",
  about = "How many periodic-schedule ticks the daemon has skipped because "
    .. "the previous tick's callback was still running, and how many of "
    .. "those skips are still consecutive right now.",
  run = function()
    local c = remuda.schedule_skips()
    return "consecutive=" .. c.consecutive .. ",total=" .. c.total
  end,
})

function remuda.process(spec)
  if type(spec.argv) ~= "table" or #spec.argv == 0 then
    error("a process needs a non-empty `argv`", 2)
  end
  if spec.on_line ~= nil and (type(spec.on_line) ~= "string" or spec.on_line == "") then
    error("a process's `on_line`, when given, must be a non-empty string", 2)
  end
  if spec.on_exit ~= nil and (type(spec.on_exit) ~= "string" or spec.on_exit == "") then
    error("a process's `on_exit`, when given, must be a non-empty string", 2)
  end
  return remuda._process_spawn(spec.argv, spec.on_line, spec.on_exit)
end
register("process", "Spawn a plain-pipe child process; its stdout lines and exit arrive as emit events.", "process(spec) -> id")
