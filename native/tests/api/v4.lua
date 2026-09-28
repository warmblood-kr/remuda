-- v4 adds screen-driven expectations to the Lua API.
-- Like earlier versions, this file becomes frozen once merged.

assert(type(remuda.expect) == "function", "remuda.expect is missing")
assert(type(remuda._expect_step) == "function", "the injectable expect matcher is missing")
assert(type(remuda._expect_option) == "function", "the label-based option picker is missing")

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
local status = remuda._expect_step(handle)
assert(status == "waiting", "expect must honor the due guard before its first capture")
now = 11
status = remuda._expect_step(handle)
assert(status == "continue", "a matching exp_continue branch must keep the expectation active")
assert(actions[1] == "working", "matched branch action must run")
screen, now = "task is ready", 12
local branch
status, branch = remuda._expect_step(handle)
assert(status == "matched" and branch == "ready", "expect must match a later branch after exp_continue")
assert(actions[2] == "task is ready", "branch action must receive the matching screen")

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
assert(remuda._expect_step(key_handle) == "waiting", "key-action expectation must start behind its due guard")
now = 15
assert(remuda._expect_step(key_handle) == "matched", "key-action branch must complete after a match")
assert(table.concat(typed, ",") == "2,RET", "key-list actions must send each requested key")
remuda.key = original_key

assert(remuda._expect_option("1. No\n2. Yes, switch to Sonnet", function(label)
  return label:lower():find("yes", 1, true) and label:lower():find("switch", 1, true)
end) == "2", "option selection must follow the label when yes moves to option 2")
assert(remuda._expect_option("1. No\n2. Keep current model", function(label)
  return label:lower():find("yes", 1, true) and label:lower():find("switch", 1, true)
end) == nil, "no matching yes/switch label must return nil")
assert(remuda._expect_option("1. Yes, switch model\n2. Yes, switch anyway", function(label)
  return label:lower():find("yes", 1, true) and label:lower():find("switch", 1, true)
end) == nil, "ambiguous matching labels must return nil")

local unknown_called = false
screen, now = "Unrecognized dialog", 12
local unknown_handle = remuda.expect("fake", { { match = "never" } }, {
  timeout = 4, now = function() return now end, capture = function() return screen end,
  unknown = function(value) return value:find("dialog", 1, true) ~= nil end,
  on_unknown = function() unknown_called = true end,
})
status = remuda._expect_step(unknown_handle)
assert(status == "waiting", "unknown-screen expectation must honor its initial due guard")
now = 14
status = remuda._expect_step(unknown_handle)
assert(status == "unknown" and unknown_called, "unmatched dialogs must call on_unknown")

local timed_out = false
screen, now = "no requested text", 20
local timeout_handle = remuda.expect("fake", { { match = "never" } }, {
  timeout = 2, now = function() return now end, capture = function() return screen end,
  on_timeout = function() timed_out = true end,
})
status = remuda._expect_step(timeout_handle)
assert(status == "waiting", "expect must wait before its deadline")
now = 23
status = remuda._expect_step(timeout_handle)
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

print("v4 expect matching ok")
