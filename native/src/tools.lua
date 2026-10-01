-- The tool vocabulary: the frame is Rust, the words are Lua.
--
-- A tool is an ordinary Lua function that has been marked exported. `mcp.rs`
-- reflects this table into `tools/list` and dispatches `tools/call` back into
-- it, so a new tool costs no rebuild and no redeploy.
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

-- Installed mods may claim a manifest-declared shell command. The core
-- forwards its remaining words here after the mod has been explicitly
-- loaded; it never imports an extension's command parser.
remuda._extension_commands = {}

-- Owner by dynamic extent (hook-design §3): while a lifecycle mod's own code
-- runs (initialize, start, and its declared hooks, tools and schedules), what
-- it registers imperatively is tagged with its name, so reload and rollback
-- can replace it like the declared registrations.
local current_owner = nil
local module_tool_owners = {}
local function with_owner(owner, fn, ...)
  local outer = current_owner
  current_owner = owner
  local result = table.pack(pcall(fn, ...))
  current_owner = outer
  if not result[1] then error(result[2], 0) end
  return table.unpack(result, 2, result.n)
end
local function timer_callback(owner, fn)
  if owner == nil then return fn end
  return function(...) return with_owner(owner, fn, ...) end
end
local native_timer_after = remuda._timer_after
local native_timer_every = remuda._timer_every
local native_timer_cancel_owner = remuda._timer_cancel_owner
remuda._timer_after = nil
remuda._timer_every = nil
remuda._timer_cancel_owner = nil
local function cancel_owner_timers(owner)
  if native_timer_cancel_owner then native_timer_cancel_owner(owner) end
end
function remuda.after(seconds, fn)
  local owner = current_owner
  return native_timer_after(seconds, timer_callback(owner, fn), owner)
end
function remuda.every(seconds, fn)
  local owner = current_owner
  return native_timer_every(seconds, timer_callback(owner, fn), owner)
end
local extension_command_owners = {}

function remuda.extension_command(name, handler)
  if type(name) ~= "string" or name == "" then error("a mod command needs a name", 2) end
  if type(handler) ~= "function" then error("a mod command needs a handler", 2) end
  remuda._extension_commands[name] = handler
  extension_command_owners[name] = current_owner
end
-- The handler's `caller.env` holds forwarded CLI environment values and must
-- not be used for authorization. Capture the native word before user Lua can
-- replace remuda.caller; its result is merged into the handler's caller data.
local native_caller = remuda.caller
function remuda._dispatch_extension_command(name, args, caller)
  local handler = remuda._extension_commands[name]
  if not handler then
    error("mod command " .. tostring(name) .. " is not loaded; run `remuda " .. tostring(name) .. "` first", 2)
  end
  local context = native_caller()
  local caller_data = type(caller) == "table" and caller or {}
  caller_data.kind = context.kind
  caller_data.session = context.session
  return handler(args or {}, caller_data)
end

-- One row per word, Rust's own bindings included (`script.rs`'s `WORDS`
-- populates this table before this file loads) — `remuda doc` reads it.
local function register(name, about, signature)
  remuda._registry[name] = { name = name, about = about, signature = signature }
end
register("after", "Run a callback once after a delay without blocking the Lua image; cancel with handle:cancel().", "after(seconds, fn) -> handle")
register("every", "Run a callback periodically without blocking the Lua image; cancel with handle:cancel(). If a callback finishes late, the next tick comes one interval after it ends, so the phase shifts and ticks do not burst to catch up.", "every(seconds, fn) -> handle")
register("tools", "The `remuda.tool` registry table, keyed by tool name.", "table")
register("_extension_commands", "Handlers registered for installed mod commands.", "table")
register("extension_command", "Register a handler for an installed mod command. Its caller table includes advisory daemon-derived kind and session fields, plus forwarded env/stdin values; kind outside does not establish operator identity.", "extension_command(name, handler(args, caller)) -> nil")
register("_dispatch_extension_command", "Dispatch arguments and caller context to a loaded mod command handler.", "_dispatch_extension_command(name, args, caller) -> value")
register("pending", "Return a bounded handle for an extension command's deferred result, including secret and visible line prompts.", "pending({timeout?, on_cancel?}) -> handle")
register("_pending_create", "Create a private pending reply handle.", "_pending_create(timeout?) -> id, handle")
register("_pending_events", "Drain pending completion and cancellation notifications.", "_pending_events() -> {{id, reason?}...}")

local pending_cancel_handlers = {}
local pending_secret_handlers = {}
local pending_line_handlers = {}
local function new_pending_handle(timeout, on_cancel)
  local id, native_handle = remuda._pending_create(timeout)
  if on_cancel then pending_cancel_handlers[id] = on_cancel end
  local handle = {}
  handle.__remuda_pending_handle = native_handle
  function handle:resolve(...) return native_handle:resolve(...) end
  function handle:reject(...) return native_handle:reject(...) end
  function handle:prompt_secret(prompt)
    if type(prompt) ~= "table" or type(prompt.label) ~= "string" or type(prompt.callback) ~= "function" then
      error("prompt_secret needs a label and callback", 2)
    end
    local prompt_id = native_handle:prompt_secret(prompt.label)
    local callbacks = pending_secret_handlers[id] or {}
    pending_secret_handlers[id] = callbacks
    callbacks[prompt_id] = prompt.callback
    return prompt_id
  end
  function handle:prompt_line(prompt)
    if type(prompt) ~= "table" or type(prompt.label) ~= "string" or type(prompt.callback) ~= "function" then
      error("prompt_line needs a label and callback", 2)
    end
    if prompt.default ~= nil and type(prompt.default) ~= "string" then
      error("prompt_line default must be a string", 2)
    end
    if prompt.preface ~= nil and type(prompt.preface) ~= "string" then
      error("prompt_line preface must be a string", 2)
    end
    local prompt_id = native_handle:prompt_line(prompt.label, prompt.default, prompt.preface)
    local callbacks = pending_line_handlers[id] or {}
    pending_line_handlers[id] = callbacks
    callbacks[prompt_id] = prompt.callback
    return prompt_id
  end
  return handle
end
function remuda.pending(options)
  if type(options) ~= "table" then
    error("pending needs an options table", 2)
  end
  local timeout = options.timeout
  if timeout ~= nil and (type(timeout) ~= "number" or timeout <= 0 or timeout > 300) then
    error("pending timeout must be a positive number no greater than 300 seconds", 2)
  end
  local on_cancel = options.on_cancel
  if on_cancel ~= nil and type(on_cancel) ~= "function" then
    error("pending on_cancel must be a function", 2)
  end
  return new_pending_handle(timeout, on_cancel)
end

local function deliver_pending_events()
  for _, event in ipairs(remuda._pending_events()) do
    local callback = pending_cancel_handlers[event.id]
    pending_cancel_handlers[event.id] = nil
    pending_secret_handlers[event.id] = nil
    pending_line_handlers[event.id] = nil
    if event.reason and callback then
      local ok, err = pcall(callback, event.reason)
      if not ok then
        io.stderr:write("remuda.pending on_cancel failed: " .. tostring(err) .. "\n")
      end
    end
  end
  for _, event in ipairs(remuda._pending_secret_events()) do
    local callbacks = pending_secret_handlers[event.id]
    local callback = callbacks and callbacks[event.prompt_id]
    if callbacks then
      callbacks[event.prompt_id] = nil
      if next(callbacks) == nil then pending_secret_handlers[event.id] = nil end
    end
    if callback then
      local ok, err = pcall(callback, event.secret, event.error)
      if not ok then
        io.stderr:write("remuda.pending prompt callback failed\n")
      end
    end
  end
  for _, event in ipairs(remuda._pending_line_events()) do
    local callbacks = pending_line_handlers[event.id]
    local callback = callbacks and callbacks[event.prompt_id]
    if callbacks then
      callbacks[event.prompt_id] = nil
      if next(callbacks) == nil then pending_line_handlers[event.id] = nil end
    end
    if callback then
      local ok, err = pcall(callback, event.line, event.error)
      if not ok then
        io.stderr:write("remuda.pending prompt callback failed\n")
      end
    end
  end
end
_G.__remuda_pending_tick = deliver_pending_events

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
  __call = function(word, arguments, caller) return word.run(arguments or {}, caller) end,
  __tostring = function(word) return "tool " .. word.name end,
}

-- Define a word and export it as an MCP tool. `args` maps each argument to a
-- description a model reads; `needs` lists the ones that are not optional.
-- Redefining an existing word replaces it.
local function make_tool(spec)
  if type(spec) ~= "table" then
    error("a tool declaration must be a table", 2)
  end
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
  if type(args) ~= "table" or type(needs) ~= "table" then
    error("tool args and needs must be tables", 2)
  end
  for key, description in pairs(args) do
    if type(key) ~= "string" or type(description) ~= "string" then
      error("tool argument names and descriptions must be strings", 2)
    end
  end
  for key in pairs(needs) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > #needs then
      error("tool needs must be a dense array", 2)
    end
  end
  for _, key in ipairs(needs) do
    if type(key) ~= "string" or args[key] == nil then
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
  return word
end

function remuda.tool(spec)
  local word = make_tool(spec)
  local owner = current_owner
  if owner then
    local run = word.run
    word.run = function(arguments, caller) return with_owner(owner, run, arguments, caller) end
  end
  remuda.tools[word.name] = word
  module_tool_owners[word.name] = owner
  register(word.name, word.about, word.name .. "(" .. arg_list(word.args, word.needs) .. ") -> string")
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
local schedule_clock_now = 0

-- Schedules are multi-registrant, using the same split as `remuda.tool`:
-- native code provides the fixed tick while this table owns the interval and
-- callback behavior. Schedules do not survive a daemon restart.
--
-- NAME is an optional label, never an identity — two mods (or one,
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
  if spec.after ~= nil and (type(spec.after) ~= "number" or spec.after < 0
    or spec.after ~= spec.after or spec.after == math.huge) then
    error("a schedule's `after`, when given, must be a finite non-negative number of seconds", 2)
  end
  if type(spec.run) ~= "function" then
    error("a schedule needs a `run` function", 2)
  end
  local owner = current_owner
  local handle = setmetatable({ name = spec.name }, Schedule)
  -- With no `after`, last_run=0 preserves the daemon-uptime-dependent first
  -- firing. An explicit delay instead anchors the first deadline at creation.
  local last_run = spec.after == nil and 0 or schedule_clock_now + spec.after - spec.every
  remuda.schedules[handle] = {
    name = spec.name,
    every = spec.every,
    run = owner and function(...) return with_owner(owner, spec.run, ...) end or spec.run,
    last_run = last_run,
    owner = owner,
  }
  return handle
end
register("schedule", "Register a periodic callback, run every `every` seconds; optional `after` sets the first firing delay from creation. Without it, the first firing depends on daemon uptime.", "schedule(spec) -> handle")

-- A no-op on an already-cancelled or unrecognized handle — a caller racing
-- its own cancel, or cancelling twice, gets silence rather than an error for
-- something that already happened.
function remuda.cancel(handle)
  remuda.schedules[handle] = nil
end
register("cancel", "Cancel a schedule by the handle `schedule()` returned.", "cancel(handle) -> nil")

local pending_expects = {}
local expect_clock_now
local function branch_matches(branch, screen)
  local matcher = branch.match
  if type(matcher) == "function" then return not not matcher(screen) end
  if type(matcher) == "string" then return screen:find(matcher) ~= nil end
  return false
end
local function run_expect_action(branch, screen, handle)
  local action = branch.action
  if type(action) == "function" then return action(screen, handle) end
  if type(action) == "string" then action = { action } end
  if type(action) == "table" then
    for _, key in ipairs(action) do remuda.key(handle.session, key) end
    return
  end
  if action ~= nil then error("expect branch action must be a function or key list", 0) end
end

-- One step is kept separate from the wake-up source: the current driver is
-- the one-second clock below, and a future PTY output event can call this same
-- function without changing remuda.expect's API.
local function expect_step(handle, now, force)
  local state, options = handle.state, handle.options
  if state.status ~= "pending" then return state.status, state.branch, state.screen end
  state.last_result = nil
  if now == nil and options.now then
    local ok, value = pcall(options.now)
    if not ok then
      state.status, state.error = "error", value
      if options.on_error then pcall(options.on_error, value, handle) end
      return state.status, nil, nil
    end
    now = value
  end
  now = now or expect_clock_now or 0
  if not state.deadline then
    state.deadline = now + handle.timeout
    state.next_at = now + (tonumber(options.interval) or 1)
    if not force then return "waiting" end
  end
  if not force and now < state.next_at then return "waiting" end
  local capture = options.capture or remuda.capture
  local captured, screen = pcall(capture, handle.session)
  if not captured then
    state.status, state.error = "error", screen
    if options.on_error then pcall(options.on_error, screen, handle) end
    return state.status, nil, nil
  end
  if type(screen) ~= "string" then
    state.status, state.error = "error", "capture did not return a screen string"
    if options.on_error then pcall(options.on_error, state.error, handle) end
    return state.status, nil, nil
  end
  local matched_disarmed = false
  for index, branch in ipairs(handle.branches) do
    local ok, matched = pcall(branch_matches, branch, screen)
    if not ok then
      state.status, state.error = "error", matched
      if options.on_error then pcall(options.on_error, matched, handle) end
      return state.status, nil, screen
    end
    if not matched and state.disarmed then
      state.disarmed[index] = nil
    elseif matched then
      if branch.continue and state.disarmed and state.disarmed[index] then
        matched_disarmed = true
      else
        local ran, err = pcall(run_expect_action, branch, screen, handle)
        if not ran then
          state.status, state.error = "error", err
          if options.on_error then pcall(options.on_error, err, handle) end
          return state.status, nil, screen
        end
        state.screen, state.branch, state.last_screen = screen, branch.id or index, screen
        if branch.continue then
          state.disarmed = state.disarmed or {}
          state.disarmed[index] = true
          state.next_at = now + (tonumber(options.interval) or 1)
          state.last_result = "continue"
          return "continue", state.branch, screen
        end
        state.status = "matched"
        return state.status, state.branch, screen
      end
    end
  end
  if not matched_disarmed then
    local unknown = options.unknown
    local unknown_ok, is_unknown = pcall(function()
      return (type(unknown) == "function" and unknown(screen))
        or (type(unknown) == "string" and screen:find(unknown) ~= nil)
    end)
    if not unknown_ok then
      state.status, state.error = "error", is_unknown
      if options.on_error then pcall(options.on_error, is_unknown, handle) end
      return state.status, nil, screen
    end
    if is_unknown then
      state.status, state.screen, state.last_screen = "unknown", screen, screen
      if options.on_unknown then
        local ok, err = pcall(options.on_unknown, screen, handle)
        if not ok then state.error = err end
      end
      return state.status, nil, screen
    end
    state.last_screen = screen
  end
  if now >= state.deadline then
    state.status, state.screen = "timeout", screen
    if options.on_timeout then
      local ok, err = pcall(options.on_timeout, screen, handle)
      if not ok then state.error = err end
    end
    return state.status, nil, screen
  end
  state.next_at = now + (tonumber(options.interval) or 1)
  return "waiting", nil, screen
end
function remuda.expect(session, branches, options)
  if type(session) ~= "string" or session == "" then error("expect needs a session name", 2) end
  if type(branches) ~= "table" or #branches == 0 then error("expect needs at least one branch", 2) end
  options = options or {}
  if type(options) ~= "table" then error("expect options must be a table", 2) end
  local timeout = tonumber(options.timeout) or 30
  local interval = tonumber(options.interval) or 1
  if timeout <= 0 or interval <= 0 then error("expect timeout and interval must be positive", 2) end
  local handle = {
    session = session,
    branches = branches,
    options = options,
    timeout = timeout,
    state = { status = "pending" },
  }
  function handle:cancel()
    if self.state.status == "pending" then self.state.status = "cancelled" end
  end
  pending_expects[#pending_expects + 1] = handle
  return handle
end
register("expect", "Watch a session asynchronously. Branches match a Lua pattern or predicate and run a key list or callback; `continue` keeps watching. Options accept a bounded timeout and unknown-screen matcher/callback.", "expect(session, branches, options?) -> handle")

local function expect_tick(now)
  expect_clock_now = now or expect_clock_now or 0
  local keep = {}
  for _, handle in ipairs(pending_expects) do
    if handle.state.status == "pending" then
      local ok, err = pcall(expect_step, handle, now)
      if not ok then
        handle.state.status, handle.state.error = "error", err
        if handle.options.on_error then pcall(handle.options.on_error, err, handle) end
      end
      if handle.state.status == "pending" then keep[#keep + 1] = handle end
    end
  end
  pending_expects = keep
end

local function expect_output(name)
  local keep = {}
  for _, handle in ipairs(pending_expects) do
    if handle.state.status == "pending" then
      if handle.session == name then
        local ok, err = pcall(expect_step, handle, expect_clock_now, true)
        if not ok then
          handle.state.status, handle.state.error = "error", err
          if handle.options.on_error then pcall(handle.options.on_error, err, handle) end
        end
      end
      if handle.state.status == "pending" then keep[#keep + 1] = handle end
    end
  end
  pending_expects = keep
end

function remuda.expect_option(screen, matches)
  if type(screen) ~= "string" or type(matches) ~= "function" then return nil end
  local found
  local markers = { "│", "┃", "❯", "›", ">" }
  for line in (screen .. "\n"):gmatch("(.-)\n") do
    line = line:gsub("^%s+", "")
    for _ = 1, #markers do
      local stripped = false
      for _, marker in ipairs(markers) do
        if line:sub(1, #marker) == marker then
          line = line:sub(#marker + 1):gsub("^%s+", "")
          stripped = true
          break
        end
      end
      if not stripped then break end
    end
    local number, label = line:match("^%s*(%d+)[%.)]%s*(.-)%s*$")
    if number and matches(label) then
      if found then return nil end
      found = number
    end
  end
  return found
end
register("expect_option", "Pick a unique numbered menu option by its label.", "expect_option(screen, label_predicate) -> number|nil")

-- Called once per native tick with the current time (seconds, native's
-- clock). Fires every schedule whose own interval has elapsed since ITS OWN
-- last run — native never sees or compares an individual interval itself.
function remuda._take_due_schedules(now)
  deliver_pending_events()
  local schedule_now = now or expect_clock_now or schedule_clock_now
  schedule_clock_now = schedule_now
  -- Expectations are advanced from the same native one-second clock. A
  -- future PTY output event may call this local directly to reduce latency.
  -- Preserve expectation-specific injected clocks: nil here lets each
  -- expectation's options.now() supply its own time, while schedule_now is
  -- the fallback clock only for periodic schedules.
  local expect_ok, expect_err = pcall(expect_tick, now)
  if not expect_ok and io and io.stderr then
    io.stderr:write("remuda.expect tick failed: " .. tostring(expect_err) .. "\n")
  end
  -- Snapshot the handles, as `emit` does: a run() that schedules must not add
  -- keys mid-`pairs` (undefined in Lua). A cancel mid-tick still takes effect.
  local handles = {}
  for handle in pairs(remuda.schedules) do
    handles[#handles + 1] = handle
  end
  local due = {}
  for _, handle in ipairs(handles) do
    local schedule = remuda.schedules[handle]
    if schedule and schedule_now - schedule.last_run >= schedule.every then
      schedule.last_run = schedule_now
      if schedule.name then
        remuda._schedule_fire_counts[schedule.name] = (remuda._schedule_fire_counts[schedule.name] or 0) + 1
      end
      due[#due + 1] = { name = schedule.name or "unnamed", handle = handle }
    end
  end
  return due
end

function remuda._run_due_schedules(now)
  for _, due in ipairs(remuda._take_due_schedules(now)) do
    local schedule = remuda.schedules[due.handle]
    if schedule then
      local ok, err = pcall(schedule.run)
      if not ok then
        io.stderr:write("remuda schedule error for " .. due.name .. ": " .. tostring(err) .. "\n")
      end
    end
  end
end

function remuda._run_schedule(handle)
  local schedule = remuda.schedules[handle]
  if not schedule then return false end
  schedule.run()
  return true
end
register(
  "_run_due_schedules",
  "Fire every schedule whose interval has elapsed. Called once per native tick.",
  "_run_due_schedules(now) -> nil"
)
register(
  "_take_due_schedules",
  "Mark and return the schedules due at `now`, for the native tick to run each under its own budget.",
  "_take_due_schedules(now) -> table"
)
register("_run_schedule", "Run one schedule by handle (native tick only).", "_run_schedule(handle) -> boolean")

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
register("hooks", "Deprecated for reading: use `hook_list`. The `remuda.on` table, keyed by event name; it becomes read-only once no mod edits it by hand.", "table")

-- Keyed by event name, counting every `emit` call for it regardless of
-- whether a hook is registered — `remuda.hooks` above only knows the events
-- someone `on`'d, not the ones only ever `emit`'d.
remuda._event_counts = {}
register("_event_counts", "Internal event-emit counts, keyed by event name. Read via `event_counts()`.", "table")

-- Emacs add-hook DEPTH: lower runs first, ties keep registration order. The
-- same (group, id) on an event replaces its hook, so `on` is idempotent.
local function source_of(fn)
  return remuda._function_source and remuda._function_source(fn) or nil
end

local function add_hook(event, entry)
  local hooks = remuda.hooks[event] or {}
  remuda.hooks[event] = hooks
  if entry.id ~= nil then
    for i = #hooks, 1, -1 do
      if hooks[i].id == entry.id and hooks[i].group == entry.group then table.remove(hooks, i) end
    end
  end
  local at = #hooks + 1
  while at > 1 and (hooks[at - 1].depth or 0) > entry.depth do at = at - 1 end
  table.insert(hooks, at, entry)
end

function remuda.on(event, fn, opts)
  if type(event) ~= "string" or event == "" then
    error("a hook needs an event name", 2)
  end
  if type(fn) ~= "function" then
    error("a hook needs a function", 2)
  end
  opts = opts or {}
  if type(opts.group) == "string" and opts.group:match("^remuda%-module:") then
    error("hook groups beginning with `remuda-module:` are reserved", 2)
  end
  if opts.depth ~= nil and type(opts.depth) ~= "number" then
    error("a hook depth must be a number", 2)
  end
  add_hook(event, { fn = fn, group = opts.group, id = opts.id, depth = opts.depth or 0,
    src = source_of(fn), errors = 0, owner = current_owner })
end
register("on", "Register a callback to run when an event fires. `opts`: `group`, `id` (same group+id replaces), `depth` (-100..100, lower first).", "on(event, fn, opts?) -> nil")

remuda.on("session_output", function(name) expect_output(name) end,
  { group = "remuda.expect", id = "session-output" })

-- A snapshot, not a live reference to `remuda.hooks[event]` — a hook that
-- calls `clear_hooks` on its own group must not skip or re-run a sibling
-- still mid-iteration.
local function snapshot(event)
  remuda._event_counts[event] = (remuda._event_counts[event] or 0) + 1
  local copy = {}
  for i, hook in ipairs(remuda.hooks[event] or {}) do copy[i] = hook end
  return copy
end

-- An error is counted on its hook and logged; the caller sees "no answer".
local function call_hook(event, hook, ...)
  local result = table.pack(pcall(hook.fn, ...))
  if result[1] then return true, table.unpack(result, 2, result.n) end
  hook.errors, hook.last_error = (hook.errors or 0) + 1, tostring(result[2])
  local who = (hook.group or hook.id) and (" [" .. tostring(hook.group) .. "/" .. tostring(hook.id) .. "]") or ""
  io.stderr:write("remuda hook error for " .. event .. who .. ": " .. hook.last_error .. "\n")
  if remuda._lifecycle_start_active then
    error(hook.last_error, 0)
  end
  return false
end

function remuda.emit(event, ...)
  for _, hook in ipairs(snapshot(event)) do call_hook(event, hook, ...) end
end
register("emit", "Fire an event, running every hook registered for it.", "emit(event, ...) -> nil")

function remuda.emit_until_success(event, ...)
  for _, hook in ipairs(snapshot(event)) do
    local ok, value = call_hook(event, hook, ...)
    if ok and value ~= nil then return value end
  end
  return nil
end
register("emit_until_success", "Fire an event until a hook returns non-nil, and return that value. An erroring hook is no answer.", "emit_until_success(event, ...) -> value?")

function remuda.emit_until_failure(event, ...)
  for _, hook in ipairs(snapshot(event)) do
    local ok, value = call_hook(event, hook, ...)
    if ok and value == false then return false end
  end
  return true
end
register("emit_until_failure", "Fire an event until a hook returns false (a veto). An erroring hook is no answer, never a veto.", "emit_until_failure(event, ...) -> boolean")

function remuda.emit_filter(event, value, ...)
  for _, hook in ipairs(snapshot(event)) do
    local ok, result = call_hook(event, hook, value, ...)
    if ok and result ~= nil then value = result end
  end
  return value
end
register("emit_filter", "Thread a value through each hook as `hook(value, ...)`; nil or an error leaves it unchanged.", "emit_filter(event, value, ...) -> value")

function remuda.hook_list(event)
  local events = {}
  for name in pairs(remuda.hooks) do
    if event == nil or name == event then events[#events + 1] = name end
  end
  table.sort(events)
  local rows = {}
  for _, name in ipairs(events) do
    for _, hook in ipairs(remuda.hooks[name]) do
      rows[#rows + 1] = { event = name, group = hook.group, id = hook.id, depth = hook.depth or 0,
        owner = hook.owner, src = hook.src, errors = hook.errors or 0, last_error = hook.last_error }
    end
  end
  return rows
end
register("hook_list", "Copies of the registered hooks, for one event or all, in run order.", "hook_list(event?) -> {{event, group, id, depth, owner, src, errors, last_error}...}")

-- advice: nadvice semantics on a function stored at a named path under
-- `remuda` (hook-design §2). The first `advise` keeps the slot's function as
-- the base and installs a trampoline; the chain runs by depth, -100
-- outermost. Locals have no address, so they are not advisable.
local advised = {} -- path -> { base, trampoline, list }
local ADVICE_KINDS = {
  around = function(fn, next, ...) return fn(next, ...) end,
  before = function(fn, next, ...) fn(...) return next(...) end,
  after = function(fn, next, ...)
    local result = table.pack(next(...))
    fn(...)
    return table.unpack(result, 1, result.n)
  end,
  override = function(fn, _, ...) return fn(...) end,
  filter_args = function(fn, next, ...) return next(fn(...)) end,
  filter_return = function(fn, next, ...) return fn(next(...)) end,
  before_while = function(fn, next, ...)
    local ok = fn(...)
    if not ok then return ok end
    return next(...)
  end,
  before_until = function(fn, next, ...)
    local early = fn(...)
    if early then return early end
    return next(...)
  end,
}

local function advice_slot(path)
  if type(path) ~= "string" or not path:match("^remuda%.[%w_%.]+$") then
    error("an advice path names a function under `remuda`, e.g. remuda._butler_notify", 3)
  end
  local parent, key = remuda, nil
  local rest = path:sub(#"remuda." + 1)
  for part in rest:gmatch("[^.]+") do
    if key ~= nil then
      parent = parent[key]
      if type(parent) ~= "table" then error("no table at " .. path, 3) end
    end
    key = part
  end
  return parent, key
end

local function run_chain(path, entry, index, ...)
  local advice = entry.list[index]
  if not advice then return entry.base(...) end
  local next = function(...) return run_chain(path, entry, index + 1, ...) end
  local result = table.pack(pcall(ADVICE_KINDS[advice.how], advice.fn, next, ...))
  if result[1] then return table.unpack(result, 2, result.n) end
  local err = result[2]
  if type(err) == "string" then
    err = err .. "\n<- advice " .. advice.id .. " (" .. advice.how .. ", depth " .. advice.depth
      .. ") on " .. path .. (advice.owner and (" [" .. advice.owner .. "]") or "")
  end
  error(err, 0)
end

-- Each installed trampoline runs a frozen copy of the chain and base it was
-- built from (Emacs-like). A caller that captured an older trampoline, such
-- as the `local orig = remuda.f; function remuda.f(...) return orig(...) end`
-- idiom, runs that older composition instead of recursing into the new one.
local trampolines = setmetatable({}, { __mode = "k" })
local function compose(path, entry)
  local frozen = { base = entry.base, list = { table.unpack(entry.list) } }
  local trampoline = function(...) return run_chain(path, frozen, 1, ...) end
  trampolines[trampoline] = true
  entry.trampoline = trampoline
  return trampoline
end

-- Put a trampoline back in the slot, adopting a function that replaced it as
-- the new base: a mod that redefines an advised function keeps its advice,
-- as `defalias` respects advice. One of our own trampolines is never adopted.
local function reattach(path, entry)
  local parent, key = advice_slot(path)
  local current = parent[key]
  if current == entry.trampoline then return end
  if current == nil then -- the function is gone, and its advice with it
    advised[path] = nil
    return
  end
  if type(current) == "function" and not trampolines[current] then entry.base = current end
  parent[key] = compose(path, entry)
end
function remuda._advice_reattach()
  for path, entry in pairs(advised) do pcall(reattach, path, entry) end
end
register("_advice_reattach", "Re-install advice trampolines over redefined functions. Called after each mod load.", "_advice_reattach() -> nil")

function remuda.advise(path, how, fn, opts)
  opts = opts or {}
  local parent, key = advice_slot(path)
  if not ADVICE_KINDS[how] then error("unknown advice kind " .. tostring(how), 2) end
  if type(fn) ~= "function" then error("advice needs a function", 2) end
  if type(opts.id) ~= "string" or opts.id == "" then error("advice needs an id", 2) end
  if opts.depth ~= nil and type(opts.depth) ~= "number" then error("an advice depth must be a number", 2) end
  local entry = advised[path]
  if not entry then
    if type(parent[key]) ~= "function" then error("no function at " .. path .. " to advise", 2) end
    entry = { list = {} }
    entry.base = parent[key]
    advised[path] = entry
  end
  for i = #entry.list, 1, -1 do
    if entry.list[i].id == opts.id then table.remove(entry.list, i) end
  end
  local advice = { id = opts.id, how = how, fn = fn, depth = opts.depth or 0, owner = current_owner }
  local at = #entry.list + 1
  while at > 1 and entry.list[at - 1].depth > advice.depth do at = at - 1 end
  table.insert(entry.list, at, advice)
  entry.trampoline = nil -- the chain changed: always compose afresh
  reattach(path, entry)
end
register("advise", "Wrap the function at a `remuda.*` path. `how`: around|before|after|override|filter_args|filter_return|before_while|before_until. `opts`: `id` (required; same id replaces), `depth` (-100 outermost).", "advise(path, how, fn, opts) -> nil")

function remuda.unadvise(path, id)
  local entry = advised[path]
  if not entry then return end
  for i = #entry.list, 1, -1 do
    if entry.list[i].id == id then table.remove(entry.list, i) end
  end
  local parent, key = advice_slot(path)
  if #entry.list == 0 then
    if trampolines[parent[key]] then parent[key] = entry.base end
    advised[path] = nil
  elseif trampolines[parent[key]] then
    parent[key] = compose(path, entry)
  end
end
register("unadvise", "Remove the advice with this id from a path; the last one removed restores the original.", "unadvise(path, id) -> nil")

function remuda.advice_member(path, id)
  for _, advice in ipairs(advised[path] and advised[path].list or {}) do
    if advice.id == id then return true end
  end
  return false
end
register("advice_member", "Whether advice with this id is on a path.", "advice_member(path, id) -> boolean")

function remuda.advice_list(path)
  local paths = {}
  for name in pairs(advised) do
    if path == nil or name == path then paths[#paths + 1] = name end
  end
  table.sort(paths)
  local rows = {}
  for _, name in ipairs(paths) do
    for _, advice in ipairs(advised[name].list) do
      rows[#rows + 1] = { path = name, id = advice.id, how = advice.how, depth = advice.depth, owner = advice.owner }
    end
  end
  return rows
end
register("advice_list", "Copies of the advice on one path or all, outermost first.", "advice_list(path?) -> {{path, id, how, depth, owner}...}")

-- For the module loader: every advice entry, to restore after a failed
-- start; and dropping one owner's advice on reload.
local function snapshot_advice()
  local saved = {}
  for path, entry in pairs(advised) do
    saved[path] = { entry = entry, base = entry.base, list = { table.unpack(entry.list) } }
  end
  return saved
end
local function restore_advice(saved)
  for path in pairs(advised) do
    if not saved[path] then
      local list = advised[path].list
      for i = #list, 1, -1 do remuda.unadvise(path, list[i].id) end
    end
  end
  for path, snap in pairs(saved) do
    snap.entry.base, snap.entry.list = snap.base, snap.list
    advised[path] = snap.entry
    -- Install the restored composition outright: the slot may hold the
    -- failed start's trampoline, which must not become the base.
    local found, parent, key = pcall(advice_slot, path)
    if found then parent[key] = compose(path, snap.entry) end
  end
end
local function drop_owned_advice(owner)
  for path, entry in pairs(advised) do
    for i = #entry.list, 1, -1 do
      if entry.list[i].owner == owner then remuda.unadvise(path, entry.list[i].id) end
    end
  end
end

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
  if type(opts.group) == "string" and opts.group:match("^remuda%-module:") then
    error("hook groups beginning with `remuda-module:` are reserved", 2)
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

-- An owned, ordered registry (VS Code `contributes`): a host defines a point,
-- extensions fill it. A mod's declared entries are replaced on reload.
local contributions = {}

-- Shallow: nested tables stay shared, but no caller can rewrite a field or
-- `order` of a stored entry, including another mod's owned one.
local function shallow_copy(entry)
  local copy = {}
  for key, value in pairs(entry) do copy[key] = value end
  return copy
end

local function contribution_problem(point, id, entry)
  if type(point) ~= "string" or point == "" then return "a contribution needs a point name" end
  if type(id) ~= "string" or id == "" then return "a contribution needs an id" end
  if type(entry) ~= "table" then return "a contribution entry must be a table" end
  if entry.order ~= nil and type(entry.order) ~= "number" then return "a contribution order must be a number" end
end

function remuda.contribute(point, id, entry)
  local problem = contribution_problem(point, id, entry)
  if problem then error(problem, 2) end
  local existing = contributions[point] and contributions[point][id]
  if existing and existing.owner ~= nil and existing.owner ~= current_owner then
    error("contribution " .. point .. "/" .. id .. " is owned by mod " .. existing.owner
      .. "; mod " .. tostring(current_owner or "<outside lifecycle>") .. " cannot replace it", 2)
  end
  contributions[point] = contributions[point] or {}
  contributions[point][id] = { owner = current_owner, entry = shallow_copy(entry) }
end
register("contribute", "Fill an extension point: the same owner can replace its entry; another mod cannot. `entry.order` sorts (default 0).", "contribute(point, id, entry) -> nil")

function remuda.contributions(point)
  local rows = {}
  for id, item in pairs(contributions[point] or {}) do
    rows[#rows + 1] = { id = id, owner = item.owner, entry = shallow_copy(item.entry) }
  end
  table.sort(rows, function(a, b)
    local left, right = a.entry.order or 0, b.entry.order or 0
    if left ~= right then return left < right end
    return a.id < b.id
  end)
  return rows
end
register("contributions", "A point's entries as {id, owner, entry} rows, entry a shallow copy, by entry.order then id.", "contributions(point) -> {{id, owner, entry}...}")

-- Opt-in lifecycle-managed mods keep their initialized state in the image and
-- declare registrations as data. Reload stages the declaration and migrations
-- before replacing the mod's hook group and tools.
local modules = {}
local field_owners = {}

-- Rollback restores the old activation's declared registrations from the
-- snapshot. Its start() must rebuild only the imperative registrations it
-- created, so discard those owner-tagged effects before running it again.
local function clear_imperative_module_registrations(name, activation)
  cancel_owner_timers(name)
  local declared_schedules, declared_tools, declared_advice, declared_contributions = {}, {}, {}, {}
  for _, handle in ipairs(activation.schedules or {}) do declared_schedules[handle] = true end
  for _, tool_name in ipairs(activation.tools or {}) do declared_tools[tool_name] = true end
  for _, advice in ipairs(activation.advice or {}) do declared_advice[advice] = true end
  for _, contribution in ipairs(activation.contributions or {}) do declared_contributions[contribution] = true end

  for event, registered in pairs(remuda.hooks) do
    local kept = {}
    for _, hook in ipairs(registered) do
      if hook.owner ~= name then kept[#kept + 1] = hook end
    end
    remuda.hooks[event] = kept
  end
  for handle, schedule in pairs(remuda.schedules) do
    if schedule.owner == name and not declared_schedules[handle] then remuda.cancel(handle) end
  end
  for tool_name, owner in pairs(module_tool_owners) do
    if owner == name and not declared_tools[tool_name] then
      remuda.tools[tool_name], remuda._registry[tool_name], module_tool_owners[tool_name] = nil, nil, nil
    end
  end
  for path, entry in pairs(advised) do
    for index = #entry.list, 1, -1 do
      local advice = entry.list[index]
      if advice.owner == name and not declared_advice[advice] then
        remuda.unadvise(path, advice.id)
      end
    end
  end
  for _, items in pairs(contributions) do
    for id, item in pairs(items) do
      if item.owner == name and not declared_contributions[item] then items[id] = nil end
    end
  end
  for command, owner in pairs(extension_command_owners) do
    if owner == name then remuda._extension_commands[command], extension_command_owners[command] = nil, nil end
  end
  for key, owner in pairs(field_owners) do
    if owner == name then remuda[key], field_owners[key] = nil, nil end
  end
end

local function stop_module_activation(name, module)
  if not module or not module.stop or module.stopped then return end
  module.stopped = true
  local ok, err = pcall(module.stop, module.state)
  if not ok then
    io.stderr:write("remuda module stop error for " .. name .. ": " .. tostring(err) .. "\n")
  end
end

-- #145: a lifecycle mod sees `remuda` through a proxy, so its assignments
-- come here. It may create new top-level fields, which it then owns; core's
-- (named in `core_fields` once this file has loaded) and another mod's are
-- refused. A field left by a legacy exec, unowned, is adopted only in the
-- mod's own namespace.
local core_fields = {}
local function set_module_field(name, key, value)
  if type(key) ~= "string" then error("mod " .. name .. " may only set string-named remuda fields", 2) end
  if core_fields[key] then
    error("mod " .. name .. " cannot replace remuda." .. key .. ", which core owns", 2)
  end
  local owner = field_owners[key]
  if owner and owner ~= name then
    error("mod " .. name .. " cannot replace remuda." .. key .. ", which mod " .. owner .. " owns", 2)
  end
  -- An unowned field that already exists (left by a legacy exec) is adopted
  -- only in the mod's own namespace, remuda._NAME_* or remuda.NAME_*.
  if not owner and remuda[key] ~= nil
    and key:sub(1, #name + 2) ~= "_" .. name .. "_" and key:sub(1, #name + 1) ~= name .. "_" then
    error("mod " .. name .. " cannot take over remuda." .. key .. ": it may adopt existing fields"
      .. " only under remuda._" .. name .. "_* or remuda." .. name .. "_*", 2)
  end
  field_owners[key] = value ~= nil and name or nil
  remuda[key] = value
end
remuda._module_set_field = set_module_field

local function clone_module_state(value, seen)
  local kind = type(value)
  if kind == "nil" or kind == "boolean" or kind == "number" or kind == "string" then
    return value
  end
  if kind ~= "table" then
    error("module state migrations only support plain tables and scalar values", 0)
  end
  if getmetatable(value) ~= nil then
    error("module state migrations cannot clone tables with metatables", 0)
  end
  seen = seen or {}
  if seen[value] then return seen[value] end
  local copy = {}
  seen[value] = copy
  for key, item in pairs(value) do
    copy[clone_module_state(key, seen)] = clone_module_state(item, seen)
  end
  return copy
end

local function array_length(value, label)
  if type(value) ~= "table" then
    error(label .. " must be an array", 0)
  end
  local length = #value
  for key in pairs(value) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > length then
      error(label .. " must be a dense array", 0)
    end
  end
  return length
end

-- REACTIVATE false is `exec`: ensure the mod is active, but leave an active
-- one (and its `start` effects) alone — only `reload` re-runs `start` (#116).
-- A `start` that failed rolls back (#129), so the next `exec` retries it.
function remuda._activate_module(name, candidate, reactivate)
  if type(name) ~= "string" or name == "" then
    error("module name must be a non-empty string", 0)
  end
  if reactivate == false and modules[name] ~= nil then
    return false
  end
  if type(candidate) ~= "table" or candidate.api ~= "remuda-module-v1" then
    error("mod entry must return a remuda-module-v1 declaration", 0)
  end
  local version = candidate.state_version
  if type(version) ~= "number" or version % 1 ~= 0 or version < 1 then
    error("module state_version must be a positive integer", 0)
  end
  if type(candidate.initialize) ~= "function" then
    error("module declaration needs an initialize function", 0)
  end
  if candidate.start ~= nil and type(candidate.start) ~= "function" then
    error("module start must be a function", 0)
  end
  if candidate.stop ~= nil and type(candidate.stop) ~= "function" then
    error("module stop must be a function", 0)
  end
  if candidate.ready ~= nil and type(candidate.ready) ~= "function" then
    error("module ready must be a function", 0)
  end
  local timeout_ms = candidate.timeout_ms
  if timeout_ms == nil then timeout_ms = 30000 end
  if candidate.ready ~= nil and (type(timeout_ms) ~= "number" or timeout_ms % 1 ~= 0
    or timeout_ms < 1 or timeout_ms > 240000) then
    error("module timeout_ms must be an integer from 1 through 240000", 0)
  end
  if candidate.ready == nil and candidate.timeout_ms ~= nil then
    error("module timeout_ms requires a ready function", 0)
  end

  local hooks = candidate.hooks or {}
  local hook_count = array_length(hooks, "module hooks")
  for index = 1, hook_count do
    local hook = hooks[index]
    if type(hook) ~= "table" or type(hook.event) ~= "string" or hook.event == ""
      or type(hook.run) ~= "function" then
      error("each module hook needs a non-empty event and run function", 0)
    end
  end

  local tools = candidate.tools or {}
  local tool_count = array_length(tools, "module tools")
  local prepared_tools, tool_names, seen_tools = {}, {}, {}
  for index = 1, tool_count do
    local declared = tools[index]
    if type(declared) ~= "table" or type(declared.run) ~= "function" then
      error("each module tool needs a run function", 0)
    end
    local spec = {
      name = declared.name,
      about = declared.about,
      args = declared.args,
      needs = declared.needs,
      run = declared.run,
    }
    local word = make_tool(spec)
    if seen_tools[word.name] then
      error("module declares duplicate tool " .. word.name, 0)
    end
    seen_tools[word.name] = true
    local existing_owner = module_tool_owners[word.name]
    if remuda.tools[word.name] ~= nil and existing_owner ~= name then
      error("tool " .. word.name .. " is already registered", 0)
    end
    prepared_tools[index] = word
    tool_names[index] = word.name
  end

  local schedules = candidate.schedules or {}
  local schedule_count = array_length(schedules, "module schedules")
  for index = 1, schedule_count do
    local schedule = schedules[index]
    if type(schedule) ~= "table" or type(schedule.every) ~= "number" or schedule.every <= 0
      or type(schedule.run) ~= "function"
      or (schedule.after ~= nil and (type(schedule.after) ~= "number" or schedule.after < 0
        or schedule.after ~= schedule.after or schedule.after == math.huge))
      or (schedule.name ~= nil and (type(schedule.name) ~= "string" or schedule.name == "")) then
      error("each module schedule needs a positive every, a run function, an optional finite non-negative after, and an optional non-empty name", 0)
    end
  end

  local declared_advice = candidate.advice or {}
  local advice_count = array_length(declared_advice, "module advice")
  for index = 1, advice_count do
    local advice = declared_advice[index]
    if type(advice) ~= "table" or type(advice.run) ~= "function" or not ADVICE_KINDS[advice.how]
      or type(advice.id) ~= "string" or advice.id == "" then
      error("each module advice needs a path, a known how, an id and a run function", 0)
    end
    local found, parent, key = pcall(advice_slot, advice.path)
    if not found or type(parent[key]) ~= "function" then
      error("module advice path " .. tostring(advice.path) .. " holds no function", 0)
    end
  end

  local contributes = candidate.contributes or {}
  if type(contributes) ~= "table" then
    error("module contributes must be a table keyed by point", 0)
  end
  local declared_contributions = {}
  for point, entries in pairs(contributes) do
    local seen = {}
    for index = 1, array_length(entries, "module contributes for " .. tostring(point)) do
      local entry = entries[index]
      local problem = contribution_problem(point, type(entry) == "table" and entry.id or nil, entry)
      if problem then error("module " .. problem, 0) end
      if seen[entry.id] then error("module declares duplicate contribution " .. point .. "/" .. entry.id, 0) end
      seen[entry.id] = true
      local existing = contributions[point] and contributions[point][entry.id]
      if existing and existing.owner ~= nil and existing.owner ~= name then
        error("contribution " .. point .. "/" .. entry.id .. " is owned by mod " .. existing.owner
          .. "; mod " .. name .. " cannot replace it", 0)
      end
    end
  end

  local migrations = candidate.migrations or {}
  if type(migrations) ~= "table" then
    error("module migrations must be a table keyed by prior state version", 0)
  end
  for from, migrate in pairs(migrations) do
    if type(from) ~= "number" or from % 1 ~= 0 or from < 1 or from >= version
      or type(migrate) ~= "function" then
      error("module migrations must map prior versions to functions", 0)
    end
  end

  local previous = modules[name]
  local state
  if previous == nil then
    state = with_owner(name, candidate.initialize)
    if type(state) ~= "table" then
      error("module initialize must return a state table", 0)
    end
  else
    if version < previous.version then
      error("module state_version cannot move backwards", 0)
    end
    state = previous.state
    for from = previous.version, version - 1 do
      local migrate = migrations[from]
      if type(migrate) ~= "function" then
        error("module is missing migration from state version " .. from, 0)
      end
      local migrated = migrate(clone_module_state(state))
      if type(migrated) ~= "table" then
        error("module migration must return a state table", 0)
      end
      state = migrated
    end
  end

  -- Stop the previous activation while it still owns its registrations and
  -- before the replacement's start can run. A cleanup error is diagnostic,
  -- not a reason to prevent reload.
  stop_module_activation(name, previous)
  cancel_owner_timers(name)

  -- Snapshot what this activation replaces, so a failing `start` can put the
  -- previous activation back (#129). State mutated by that `start` stays.
  local saved_hooks, saved_tools, saved_schedules, saved_owner_schedules, saved_commands = {}, {}, {}, {}, {}
  local saved_advice = snapshot_advice()
  local saved_fields = {}
  for key, owner in pairs(field_owners) do
    if owner == name then saved_fields[key] = remuda[key] end
  end
  for event, registered in pairs(remuda.hooks) do
    saved_hooks[event] = { table.unpack(registered) }
  end
  for _, tool_name in ipairs(tool_names) do
    saved_tools[tool_name] = { remuda.tools[tool_name], remuda._registry[tool_name], module_tool_owners[tool_name] }
  end
  for _, tool_name in ipairs(previous and previous.tools or {}) do
    saved_tools[tool_name] = { remuda.tools[tool_name], remuda._registry[tool_name], module_tool_owners[tool_name] }
  end
  for tool_name, owner in pairs(module_tool_owners) do
    if owner == name then
      saved_tools[tool_name] = { remuda.tools[tool_name], remuda._registry[tool_name], owner }
    end
  end
  for _, handle in ipairs(previous and previous.schedules or {}) do
    saved_schedules[handle] = remuda.schedules[handle]
  end
  for handle, schedule in pairs(remuda.schedules) do
    if schedule.owner == name then saved_owner_schedules[handle] = true end
  end
  local saved_contributions = {}
  for point, items in pairs(contributions) do
    saved_contributions[point] = {}
    for id, item in pairs(items) do saved_contributions[point][id] = item end
  end

  for command, owner in pairs(extension_command_owners) do
    if owner == name then saved_commands[command] = remuda._extension_commands[command] end
  end

  -- Everything the mod owns goes: its reserved group (declared hooks) and
  -- whatever it registered imperatively in its own extent.
  local group = "remuda-module:" .. name
  local function owned(hook) return hook.group == group or hook.owner == name end
  for command in pairs(saved_commands) do
    remuda._extension_commands[command], extension_command_owners[command] = nil, nil
  end
  drop_owned_advice(name)
  -- The mod's fields go too; `start` recreates the ones it still defines,
  -- and advice on those re-attaches (a field not recreated takes its advice).
  for key in pairs(saved_fields) do
    remuda[key], field_owners[key] = nil, nil
  end
  for index = 1, advice_count do
    local advice = declared_advice[index]
    with_owner(name, remuda.advise, advice.path, advice.how, function(...)
      return with_owner(name, advice.run, state, ...)
    end, { id = advice.id, depth = advice.depth })
  end
  for event, registered in pairs(remuda.hooks) do
    local kept = {}
    for _, hook in ipairs(registered) do
      if not owned(hook) then
        kept[#kept + 1] = hook
      end
    end
    remuda.hooks[event] = kept
  end
  for _, old_name in ipairs(previous and previous.tools or {}) do
    if module_tool_owners[old_name] == name then
      remuda.tools[old_name] = nil
      remuda._registry[old_name] = nil
      module_tool_owners[old_name] = nil
    end
  end
  for index = 1, hook_count do
    local hook = hooks[index]
    add_hook(hook.event, { fn = function(...)
      return with_owner(name, hook.run, state, ...)
    end, group = group, id = hook.id, depth = type(hook.depth) == "number" and hook.depth or 0,
      src = source_of(hook.run), errors = 0 })
  end
  for index, word in ipairs(prepared_tools) do
    local declared = tools[index]
    word.run = function(arguments, caller)
      return with_owner(name, declared.run, state, arguments, caller)
    end
    remuda.tools[word.name] = word
    module_tool_owners[word.name] = name
    register(word.name, word.about, word.name .. "(" .. arg_list(word.args, word.needs) .. ") -> string")
  end
  for _, handle in ipairs(previous and previous.schedules or {}) do
    remuda.cancel(handle)
  end
  for _, items in pairs(contributions) do
    for id, item in pairs(items) do
      if item.owner == name then items[id] = nil end
    end
  end
  for point, entries in pairs(contributes) do
    contributions[point] = contributions[point] or {}
    for _, entry in ipairs(entries) do
      local bound = {}
      for key, field in pairs(entry) do
        bound[key] = type(field) == "function" and function(...) return field(state, ...) end or field
      end
      local contribution = { owner = name, entry = bound }
      contributions[point][entry.id] = contribution
      declared_contributions[#declared_contributions + 1] = contribution
    end
  end
  local schedule_handles = {}
  for index = 1, schedule_count do
    local declared = schedules[index]
    schedule_handles[index] = with_owner(name, remuda.schedule, {
      name = declared.name,
      every = declared.every,
      after = declared.after,
      run = function() return with_owner(name, declared.run, state) end,
    })
  end
  local stop = candidate.stop and function(stopped_state)
    return with_owner(name, candidate.stop, stopped_state)
  end
  local start = candidate.start and function(started_state)
    return with_owner(name, candidate.start, started_state)
  end
  local ready = candidate.ready and function(ready_state)
    return with_owner(name, candidate.ready, ready_state)
  end
  local activation = {
    version = version,
    state = state,
    tools = tool_names,
    schedules = schedule_handles,
    advice = {},
    contributions = declared_contributions,
    stop = stop,
    start = start,
    ready = ready,
    timeout_ms = timeout_ms,
    stopped = false,
  }
  for index = 1, advice_count do
    local declared = declared_advice[index]
    local entry = advised[declared.path]
    for _, advice in ipairs(entry and entry.list or {}) do
      if advice.owner == name and advice.id == declared.id then
        activation.advice[#activation.advice + 1] = advice
        break
      end
    end
  end
  modules[name] = activation

  local function rollback()
    stop_module_activation(name, activation)
    cancel_owner_timers(name)
    for event, registered in pairs(remuda.hooks) do
      local restored = saved_hooks[event] or {}
      local known = {}
      for _, hook in ipairs(restored) do known[hook] = true end
      for _, hook in ipairs(registered) do
        if not owned(hook) and not known[hook] then
          restored[#restored + 1] = hook
        end
      end
      -- Survivors were appended; put them back by depth. table.sort is not
      -- stable, so ties fall back to their position.
      local position = {}
      for index, hook in ipairs(restored) do position[hook] = index end
      table.sort(restored, function(a, b)
        local left, right = a.depth or 0, b.depth or 0
        if left ~= right then return left < right end
        return position[a] < position[b]
      end)
      remuda.hooks[event] = restored
    end
    for tool_name, saved in pairs(saved_tools) do
      remuda.tools[tool_name], remuda._registry[tool_name], module_tool_owners[tool_name] =
        saved[1], saved[2], saved[3]
    end
    for _, handle in ipairs(schedule_handles) do
      remuda.cancel(handle)
    end
    -- A failing start can register schedules imperatively. They have the
    -- module owner but are absent from the declaration's schedule_handles.
    -- Preserve schedules that existed before activation; the previous
    -- activation's restart path will rebuild its own imperative schedules.
    for handle, schedule in pairs(remuda.schedules) do
      if schedule.owner == name and not saved_owner_schedules[handle] then
        remuda.cancel(handle)
      end
    end
    for handle, schedule in pairs(saved_schedules) do
      remuda.schedules[handle] = schedule
    end
    for command, owner in pairs(extension_command_owners) do
      if owner == name then remuda._extension_commands[command], extension_command_owners[command] = nil, nil end
    end
    for command, handler in pairs(saved_commands) do
      remuda._extension_commands[command], extension_command_owners[command] = handler, name
    end
    for key, owner in pairs(field_owners) do
      if owner == name then remuda[key], field_owners[key] = nil, nil end
    end
    for key, value in pairs(saved_fields) do
      remuda[key], field_owners[key] = value, name
    end
    restore_advice(saved_advice)
    for point, items in pairs(contributions) do
      local restored = saved_contributions[point] or {}
      for id, item in pairs(items) do
        if item.owner ~= name and restored[id] == nil then restored[id] = item end
      end
      contributions[point] = restored
    end
    modules[name] = previous
    if previous and previous.stopped and previous.start then
      clear_imperative_module_registrations(name, previous)
      local was_active = remuda._lifecycle_start_active
      remuda._lifecycle_start_active = true
      local ok, err = pcall(previous.start, previous.state)
      remuda._lifecycle_start_active = was_active
      if ok then
        previous.stopped = false
      else
        io.stderr:write("remuda module restart error for " .. name .. ": " .. tostring(err) .. "\n")
        for event, registered in pairs(remuda.hooks) do
          local kept = {}
          for _, hook in ipairs(registered) do
            if not owned(hook) then kept[#kept + 1] = hook end
          end
          remuda.hooks[event] = kept
        end
        for _, tool_name in ipairs(previous.tools) do
          if module_tool_owners[tool_name] == name then
            remuda.tools[tool_name], remuda._registry[tool_name], module_tool_owners[tool_name] = nil, nil, nil
          end
        end
        for _, handle in ipairs(previous.schedules) do remuda.cancel(handle) end
        for command, owner in pairs(extension_command_owners) do
          if owner == name then
            remuda._extension_commands[command], extension_command_owners[command] = nil, nil
          end
        end
        for key, owner in pairs(field_owners) do
          if owner == name then remuda[key], field_owners[key] = nil, nil end
        end
        drop_owned_advice(name)
        for _, items in pairs(contributions) do
          for id, item in pairs(items) do
            if item.owner == name then items[id] = nil end
          end
        end
        modules[name] = nil
      end
    end
  end
  return true, state, start, rollback
end

-- Called by the daemon's clean shutdown path. A snapshot avoids mutation
-- hazards if a stop callback activates another module.
function remuda._stop_modules()
  local active = {}
  for name, module in pairs(modules) do active[#active + 1] = { name, module } end
  for _, pair in ipairs(active) do
    local name, module = pair[1], pair[2]
    stop_module_activation(name, module)
    cancel_owner_timers(name)
  end
end

-- The CLI polls this small lifecycle result between Eval requests. Keep the
-- readiness callback in the declaration table, not as another public remuda
-- word; an absent callback preserves today's immediate exec completion.
function remuda._module_readiness(name)
  local module = modules[name]
  if not module or not module.ready then return { status = "ready" } end
  local ok, ready, message = pcall(module.ready, module.state)
  if not ok then return { status = "failed", message = tostring(ready) } end
  if ready == true and message == nil then return { status = "ready" } end
  if ready == nil and message == nil then
    return { status = "pending", timeout_ms = module.timeout_ms }
  end
  if ready == nil and type(message) == "string" then
    return { status = "failed", message = message }
  end
  return {
    status = "failed",
    message = "ready must return true, nil, or nil, message",
  }
end
register("_module_readiness", "Internal readiness poll for remuda exec.", "_module_readiness(name) -> {status, timeout_ms?, message?}")

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

-- Deprecated flat spellings keep resolving dynamically through this table.
-- That matters for API v1-v4 callers which wrap/replace a flat function path:
-- namespace words still pass through the old slot during the compatibility
-- window. The alias itself calls the captured primitive, avoiding a cycle.
local deprecated_notices = {}
local through_namespace = {}
local function call_flat(name, ...)
  local prior = through_namespace[name]
  through_namespace[name] = true
  local result = table.pack(pcall(remuda[name], ...))
  through_namespace[name] = prior
  if not result[1] then error(result[2], 0) end
  return table.unpack(result, 2, result.n)
end

local function deprecated_alias(name, replacement, primitive)
  return function(...)
    if not through_namespace[name]
      and os.getenv("REMUDA_SUPPRESS_DEPRECATIONS") ~= "1"
      and not deprecated_notices[name] then
      deprecated_notices[name] = true
      io.stderr:write("deprecated: remuda." .. name .. "; use remuda.session." .. replacement .. "\n")
    end
    return primitive(...)
  end
end

local flat_session_words = {
  ls = remuda.ls,
  new = remuda.new,
  close = remuda.close,
  attach = remuda.attach,
}

local Buffer = {}
Buffer.__index = Buffer

remuda.buffer = {}
register("buffer", "Namespace for creating and listing named text buffers.", "table")

-- Create-if-absent, return-if-present — so two mods naming the same
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
-- itself, so nothing here duplicates what `remuda.session.list()` reports.
local Session = {}
Session.__index = function(self, key)
  if key == "buffer" then
    return remuda.buffer.new(self.name)
  elseif key == "is_busy" then
    for _, row in ipairs(remuda.session.list()) do
      if row.name == self.name then
        -- No output for a couple of seconds is a useful working/idle heuristic.
        -- `row.idle` remains since-input for callers that use that measure.
        return row.output_idle < 2.0
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
local function session_handle(name)
  if type(name) ~= "string" or name == "" then
    error("a session needs a name", 2)
  end
  return setmetatable({ name = name }, Session)
end

remuda.session = {
  list = function(...) return call_flat("ls", ...) end,
  new = function(...) return call_flat("new", ...) end,
  close = function(...) return call_flat("close", ...) end,
  attach = function(...) return call_flat("attach", ...) end,
  resize = function(...) return remuda._session_resize(...) end,
}
setmetatable(remuda.session, {
  __call = function(_, name) return session_handle(name) end,
})
register("session", "Calling remuda.session(name) returns a handle onto that named session; the namespace also provides list, new, close, attach and resize.",
  "session(name) -> handle; table {list, new, close, attach, resize}")
register("session.list", "List every session in the registry, reaping exited ones unless REMUDA_KEEP_EXITED is set.", "session.list() -> {session...}")
register("session.new", "Start a session, defaulting the command to the user's shell.",
  "session.new(name?, argv?, cwd?, env?) -> string")
register("session.close", "End a session, live or already self-exited.", "session.close(name) -> nil")
register("session.attach", "Enter raw mode on a session.", "session.attach(name) -> nil")
register("session.resize", "Resize a session's terminal (cols 20..1000, rows 24..500).", "session.resize(name, cols, rows) -> true | nil, err")

remuda.ls = deprecated_alias("ls", "list", flat_session_words.ls)
remuda.new = deprecated_alias("new", "new", flat_session_words.new)
remuda.close = deprecated_alias("close", "close", flat_session_words.close)
remuda.attach = deprecated_alias("attach", "attach", flat_session_words.attach)
register("ls", "Deprecated alias for `remuda.session.list`.", "ls() -> {session...}")
register("new", "Deprecated alias for `remuda.session.new`.", "new(name?, argv?, cwd?, env?) -> string")
register("close", "Deprecated alias for `remuda.session.close`.", "close(name) -> nil")
register("attach", "Deprecated alias for `remuda.session.attach`.", "attach(name) -> nil")

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
function remuda._call(name, arguments, caller)
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
  -- `caller` is daemon-issued process context, never MCP input.  Existing
  -- tools keep working because Lua ignores the optional second argument.
  local answer = word(arguments, caller)
  if answer == nil then
    return ""
  end
  if type(answer) == "table" and rawget(answer, "__remuda_pending_handle") ~= nil then
    return answer
  end
  return tostring(answer)
end
register("_call", "Dispatch one MCP tools/call by name.", "_call(name, arguments, caller) -> string")

-- Input is expressed as two words: one contiguous text burst, then a
-- separately-timed submit key after the composer shows the text.
remuda.input = {}

function remuda.input.text(session, text)
  remuda._input_text(session, tostring(text))
end

register("input", "Terminal input words for text delivery and submission.", "table")
register("input.text", "Deliver text as one burst, using bracketed paste when enabled by the child.", "input.text(session, text) -> nil")

function remuda.input.submit(session, expect)
  return remuda._input_submit(session, tostring(expect))
end

register("input.submit", "Submit visible composer text; returns 'submitted' or 'unverified'.", "input.submit(session, expect) -> status")

-- Composite: hold the session input lock across text, settle, and submission.
function remuda.type_text(session, text, settle)
  return remuda._input_type_text(session, tostring(text), settle or 0.1)
end
remuda.input.type_text = remuda.type_text
register("input.type_text", "Type text, honor the settle pause, then return 'submitted' or 'unverified'.", "input.type_text(session, text, settle?) -> status")
register(
  "type_text",
  "Type text into a session and submit it with Return; returns 'submitted' or 'unverified'.",
  "type_text(session, text, settle?) -> status"
)

-- The left session list, re-expressed as the "*sessions*" buffer instead of
-- being drawn straight out of Rust.
--
-- `tui.rs` keeps the cursor mark only: which row is selected is a per-viewer
-- fact, not buffer content.  Everything else people see in the session list
-- — status color, detail, and the breathing room between sessions — belongs
-- here, where a live Lua image can revise it without rebuilding the TUI.
--
-- WIDTH travels with the bridge call so any budget-aware row detail can be
-- chosen here; Rust still fits the resulting rows to the caller's pane.
function remuda._refresh_sessions_buffer(width, selected, selected_name)
  local sessions = remuda.session.list()
  local ordered = sessions
  local has_order = false
  -- The private order line is tab-delimited. A daemon normally receives names
  -- from a shell/CLI, but its protocol permits arbitrary strings, so verify
  -- the framing assumption before opting into a reordered render.
  local names_are_frameable = true
  for _, session in ipairs(sessions) do
    if session.name:find("[\t\r\n]") then names_are_frameable = false break end
  end
  if names_are_frameable and type(remuda.session_order) == "function" then
    local ok, requested = pcall(remuda.session_order, sessions)
    if ok and type(requested) == "table" then
      local live, seen, reordered = {}, {}, {}
      for _, session in ipairs(sessions) do live[session.name] = session end
      for _, entry in ipairs(requested) do
        local name = type(entry) == "table" and entry.name
        local session = type(name) == "string" and live[name]
        if session and not seen[name] then
          local depth = tonumber(entry.depth) or 0
          depth = math.max(0, math.floor(depth))
          reordered[#reordered + 1] = { session = session, depth = depth }
          seen[name] = true
        end
      end
      for _, session in ipairs(sessions) do
        if not seen[session.name] then
          reordered[#reordered + 1] = { session = session, depth = 0 }
        end
      end
      ordered, has_order = reordered, true
    end
  end
  local lines = {}
  -- One entry is a block of rows laid out at x = 0; nesting moves the whole
  -- block, so no row can keep a position of its own.
  local function place(block, dx)
    local pad = string.rep(" ", dx)
    for i, row in ipairs(block) do block[i] = pad .. row end
    return block
  end
  local function session_detail(session)
    if type(remuda.session_detail) ~= "function" then return nil end
    return remuda.session_detail(session)
  end
  if #sessions == 0 then
    lines = { "the herd is empty.", "press n to start a session." }
  else
    for i, item in ipairs(ordered) do
      local s = item.session or item
      local depth = item.depth or 0
      local detail = session_detail(s)
      local state_color = s.alive and "\27[32m" or "\27[31m"
      local reset = "\27[0m"
      local base = (i - 1) * 3
      -- Names remain neutral and readable; state carries the color. Keeping
      -- an actually blank third row gives entries whitespace rather than a
      -- second competing visual treatment.
      local is_selected = selected_name and s.name == selected_name
        or (not selected_name and i - 1 == selected)
      -- Reverse video marks the selection without a caret column.
      local name_style = is_selected and "\27[1;7;36m" or "\27[1m"
      -- The dot carries live/dead; row 2 is reserved for telemetry.
      -- Each style resets before the next starts, so none bleeds into another.
      -- U+25CF BLACK CIRCLE. Ambiguous width; tui.rs's char_width counts it
      -- as one cell by owner choice.
      local dot = "●"
      local marker = s.attached and " 🏇" or ""
      local parts = {}
      if detail then parts[#parts + 1] = "\27[2m" .. detail .. reset end
      local block = place({
        name_style .. s.name .. reset .. " " .. state_color .. dot .. reset .. marker,
        table.concat(parts, "  "),
        "",
      }, 2 * depth)
      for r, row in ipairs(block) do lines[base + r] = row end
    end
  end
  -- The private first line is the native bridge contract: Lua selects the
  -- number of rows and all visual treatment, while Rust only maps input and
  -- viewport positions onto those rows.
  table.insert(lines, 1, "\30" .. tostring(3))
  -- A successful ordering hook adds a second private line for Rust.
  if has_order then
    local names = {}
    for _, item in ipairs(ordered) do
      local name = (item.session or item).name
      names[#names + 1] = name
    end
    table.insert(lines, 2, "\31" .. table.concat(names, "\t"))
  end
  remuda.buffer.new("*sessions*"):set(table.concat(lines, "\n"))
end
register(
  "_refresh_sessions_buffer",
  "Rebuild the *sessions* buffer's content.",
  "_refresh_sessions_buffer(width, selected) -> nil"
)

-- Render the live registry for the manual command. The registry is the source
-- of truth for every host-bound and Lua-defined word; format changes only affect
-- presentation.
local function registry_rows()
  local rows = {}
  for _, name in ipairs(sorted_keys(remuda._registry)) do
    if name:sub(1, 1) ~= "_" then
      local word = remuda._registry[name]
      rows[#rows + 1] = {
        name = word.name,
        signature = word.signature,
        description = word.about,
        kind = word.signature == "table" and "variable" or "function",
      }
    end
  end
  return rows
end

local function registry_json(rows)
  local functions, variables = {}, {}
  for _, row in ipairs(rows) do
    local encoded = '{"name":' .. quoted(row.name)
      .. ',"signature":' .. quoted(row.signature)
      .. ',"description":' .. quoted(row.description) .. '}'
    if row.kind == "variable" then
      variables[#variables + 1] = encoded
    else
      functions[#functions + 1] = encoded
    end
  end
  return '{"name":"remuda","runtime":{"functions":['
    .. table.concat(functions, ",")
    .. '],"classes":[],"variables":['
    .. table.concat(variables, ",") .. ']}}'
end

local function registry_markdown(rows)
  local lines = {"# Remuda Lua runtime", ""}
  for _, row in ipairs(rows) do
    lines[#lines + 1] = "## `" .. row.name .. "`"
    lines[#lines + 1] = ""
    lines[#lines + 1] = "`" .. row.signature .. "` — " .. row.description
    lines[#lines + 1] = ""
  end
  return table.concat(lines, "\n")
end

local function registry_rst(rows)
  local lines = {"Remuda Lua runtime", "==================", ""}
  for _, row in ipairs(rows) do
    lines[#lines + 1] = row.name
    lines[#lines + 1] = string.rep("-", #row.name)
    lines[#lines + 1] = ""
    lines[#lines + 1] = "``" .. row.signature .. "`` — " .. row.description
    lines[#lines + 1] = ""
  end
  return table.concat(lines, "\n")
end

function remuda._registry_dump(format)
  local rows = registry_rows()
  if format == "json" then return registry_json(rows) end
  if format == "markdown" then return registry_markdown(rows) end
  if format == nil or format == "rst" then return registry_rst(rows) end
  error("unknown documentation format: " .. tostring(format), 0)
end
register("_registry_dump", "Render the live word registry as documentation.", "_registry_dump(format?) -> string")

-- The first word, and the one `steps/008` found missing: readiness. Driving an
-- agent means waiting for it, and every caller so far has written this loop
-- again — `tests/api/v1.lua` has its own copy.
remuda.tool({
  name = "wait_for",
  about = "Wait until a session's screen matches a Lua pattern, then answer with "
    .. "that screen. Fails when the deadline passes instead of answering with a "
    .. "screen that does not match. Use it after `send` rather than guessing a delay.",
  args = {
    session = "The session to watch.",
    pattern = "A Lua pattern the screen must match. `%$ %s*$` is a shell prompt.",
    seconds = "How long to wait before giving up. Default 30; positive and at most 300.",
  },
  needs = { "session", "pattern" },
  run = function(a)
    -- MCP argument values arrive as strings; every schema this frame emits says
    -- so. A number is what this one means, and `tonumber` is where that is said.
    local seconds = tonumber(a.seconds) or 30
    if seconds ~= seconds or seconds <= 0 or seconds > 300 then
      error("wait_for seconds must be positive and no greater than 300", 0)
    end
    local deadline = remuda.clock() + seconds * 1000
    local screen = remuda.capture(a.session)
    if branch_matches({ match = a.pattern }, screen) then
      return screen
    end

    local timer
    local pending = new_pending_handle(seconds + 1, function()
      if timer then timer:cancel() end
    end)
    local function reject_deadline()
      pending:reject(string.format(
        "%s never matched %q within %gs. last screen:\n%s",
        a.session,
        a.pattern,
        seconds,
        screen or ""
      ))
    end
    local poll
    poll = function()
      timer = nil
      local ok, latest = pcall(remuda.capture, a.session)
      if not ok then
        pending:reject(tostring(latest))
        return
      end
      screen = latest
      if branch_matches({ match = a.pattern }, screen) then
        pending:resolve(0, screen, "")
        return
      end
      local remaining = (deadline - remuda.clock()) / 1000
      if remaining <= 0 then
        reject_deadline()
        return
      end
      timer = remuda.after(math.max(0.01, math.min(0.1, remaining)), poll)
    end
    timer = remuda.after(math.max(0.01, math.min(0.1, seconds)), poll)
    return pending
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

local function process_start(spec)
  if type(spec) ~= "table" then error("a process needs a spec table", 2) end
  if type(spec.argv) ~= "table" or #spec.argv == 0 then
    error("a process needs a non-empty `argv`", 2)
  end
  for i, arg in ipairs(spec.argv) do
    if type(arg) ~= "string" or (i == 1 and arg == "") then
      error("a process argv must contain strings and a non-empty executable", 2)
    end
  end
  if spec.on_line ~= nil and (type(spec.on_line) ~= "string" or spec.on_line == "") then
    error("a process's `on_line`, when given, must be a non-empty string", 2)
  end
  if spec.on_exit ~= nil and (type(spec.on_exit) ~= "string" or spec.on_exit == "") then
    error("a process's `on_exit`, when given, must be a non-empty string", 2)
  end
  return remuda._process_spawn(spec.argv, spec.on_line, spec.on_exit)
end
local function process_run(spec)
  if type(spec) ~= "table" then error("process.run needs a spec table", 2) end
  if type(spec.argv) ~= "table" or #spec.argv == 0 then
    error("process.run needs a non-empty `argv`", 2)
  end
  for i, arg in ipairs(spec.argv) do
    if type(arg) ~= "string" or (i == 1 and arg == "") then
      error("process.run argv must contain strings and a non-empty executable", 2)
    end
  end
  if spec.stdin ~= nil and type(spec.stdin) ~= "string" then
    error("process.run stdin must be a string", 2)
  end
  local timeout = spec.timeout
  if timeout == nil then timeout = 5 end
  if type(timeout) ~= "number" or timeout ~= timeout or timeout <= 0 or timeout > 30 then
    error("process.run timeout must be positive and at most 30 seconds", 2)
  end
  return remuda._process_run(spec.argv, spec.stdin, timeout)
end
remuda.process = setmetatable({ run = process_run }, {
  __call = function(_, spec) return process_start(spec) end,
})
register("process", "Spawn an asynchronous plain-pipe child; process.run executes argv synchronously with bounded timeout and output.", "process(spec) -> id; process.run(spec) -> {code, stdout, stderr, timed_out}")
register("process.run", "Run argv directly without a shell; inherits the daemon's environment and working directory. Blocks the Lua image until exit or timeout (default 5s, max 30s), captures each stream up to 1 MiB. Surviving descendants can keep pipes open; at most 16 background output readers are allowed.", "process.run{argv, stdin?, timeout?} -> {code, stdout, stderr, timed_out, signal?}")

-- Everything defined so far is core's; a mod may not replace it (#145).
for key in pairs(remuda) do core_fields[key] = true end
