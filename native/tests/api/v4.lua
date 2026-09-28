-- v4 adds screen-driven expectations to the Lua API.
-- Like earlier versions, this file becomes frozen once merged.

assert(type(remuda.expect) == "function", "remuda.expect is missing")
assert(type(remuda.expect_option) == "function", "the label-based option picker is missing")
assert(remuda._expect_step == nil and remuda._expect_tick == nil,
  "expectation implementation helpers must remain private")

local function step(handle, now)
  remuda._run_due_schedules(now)
  local state = handle.state
  return state.status == "pending" and (state.last_result or "waiting") or state.status,
    state.branch, state.screen
end

local now, screen, actions = 10, "still working", {}
local opts = {
  timeout = 4,
  now = function() return now end,
  capture = function(session)
    assert(session == "fake", "expect must capture the requested session")
    return screen
  end,
}
local branches = {
  { id = "working", match = "still working", action = function() actions[#actions + 1] = "working" end, continue = true },
  { id = "ready", match = "ready", action = function(value) actions[#actions + 1] = value end },
}

local handle = remuda.expect("fake", branches, opts)
local status = step(handle, now)
assert(status == "waiting", "expect must honor the due guard before its first capture")
now = 11
status = step(handle, now)
assert(status == "continue", "a matching exp_continue branch must keep the expectation active")
assert(actions[1] == "working", "matched branch action must run")
screen, now = "task is ready", 12
local branch
status, branch = step(handle, now)
assert(status == "matched" and branch == "ready", "expect must match a later branch after exp_continue")
assert(actions[2] == "task is ready", "branch action must receive the matching screen")

-- A continue branch is edge-triggered: volatile screen changes do not replay
-- it, but one nonmatching capture rearms it for the next occurrence.
local edges, edge_screen, edge_now = 0, "working 00:01", 40
local edge_handle = remuda.expect("fake", {
  { match = "working", continue = true, action = function() edges = edges + 1 end },
}, { timeout = 20, now = function() return edge_now end, capture = function() return edge_screen end })
step(edge_handle, edge_now)
edge_now = 41; step(edge_handle, edge_now)
edge_screen, edge_now = "working 00:02", 42; step(edge_handle, edge_now)
assert(edges == 1, "volatile changes in a matching screen must not replay continue actions")
edge_screen, edge_now = "idle", 43; step(edge_handle, edge_now)
edge_screen, edge_now = "working 00:03", 44; step(edge_handle, edge_now)
assert(edges == 2, "a nonmatch must rearm the continue branch")

local bad_clock = remuda.expect("fake", { { match = "x" } }, {
  now = function() error("clock failed") end, capture = function() return "x" end,
})
local bad_clock_status = step(bad_clock)
assert(bad_clock_status == "error", "throwing injected clocks must become expectation errors")

local typed = {}
local original_key = remuda.key
remuda.key = function(session, key)
  assert(session == "fake", "key action must target the expected session")
  typed[#typed + 1] = key
end
screen, now = "confirmation prompt", 14
local key_handle = remuda.expect("fake", {
  { id = "confirm", match = "confirmation", action = { "2", "RET" } },
}, opts)
assert(step(key_handle, now) == "waiting", "key-action expectation must start behind its due guard")
now = 15
assert(step(key_handle, now) == "matched", "key-action branch must complete after a match")
assert(table.concat(typed, ",") == "2,RET", "key-list actions must send each requested key")
remuda.key = original_key

assert(remuda.expect_option("1. No\n2. Yes, switch to Sonnet", function(label)
  return label:lower():find("yes", 1, true) and label:lower():find("switch", 1, true)
end) == "2", "option selection must follow the label when yes moves to option 2")
assert(remuda.expect_option("1. No\n2. Keep current model", function(label)
  return label:lower():find("yes", 1, true) and label:lower():find("switch", 1, true)
end) == nil, "no matching yes/switch label must return nil")
assert(remuda.expect_option("1. Yes, switch model\n2. Yes, switch anyway", function(label)
  return label:lower():find("yes", 1, true) and label:lower():find("switch", 1, true)
end) == nil, "ambiguous matching labels must return nil")

local unknown_called = false
screen, now = "Unrecognized dialog", 12
local unknown_handle = remuda.expect("fake", { { match = "never" } }, {
  timeout = 4, now = function() return now end, capture = function() return screen end,
  unknown = function(value) return value:find("dialog", 1, true) ~= nil end,
  on_unknown = function() unknown_called = true end,
})
status = step(unknown_handle, now)
assert(status == "waiting", "unknown-screen expectation must honor its initial due guard")
now = 14
status = step(unknown_handle, now)
assert(status == "unknown" and unknown_called, "unmatched dialogs must call on_unknown")

local timed_out = false
screen, now = "no requested text", 20
local timeout_handle = remuda.expect("fake", { { match = "never" } }, {
  timeout = 2, now = function() return now end, capture = function() return screen end,
  on_timeout = function() timed_out = true end,
})
status = step(timeout_handle, now)
assert(status == "waiting", "expect must wait before its deadline")
now = 23
status = step(timeout_handle, now)
assert(status == "timeout" and timed_out, "expect must report a bounded timeout")

screen = "scheduled branch matched"
local scheduled = remuda.expect("fake", { { id = "scheduled", match = "scheduled branch" } }, {
  timeout = 3, capture = function() return screen end,
})
remuda._run_due_schedules(30)
assert(scheduled.state.status == "pending", "clock tick must respect the first due guard")
remuda._run_due_schedules(31)
assert(scheduled.state.status == "matched" and scheduled.state.branch == "scheduled",
  "the existing clock driver must advance pending expectations")

local schedule_ran = false
local schedule = remuda.schedule({ every = 1, run = function() schedule_ran = true end })
local tick_ok = pcall(remuda._run_due_schedules, 32)
remuda.cancel(schedule)
assert(tick_ok and schedule_ran, "an expectation failure must not prevent schedules from running")

print("v4 expect matching ok")
