-- v2.lua: widens v1 with `remuda.feed` and `remuda.tools.type_text`.
--
-- Like v1.lua, this file is FROZEN once merged — widening the API again means
-- adding `v3.lua` next to this file, never editing this one.
-- 2026-10-01: owner-approved break: remuda.sleep was deleted (owner decision 2026-09-30); calling it raises the use-remuda.after error.

local name = "api-v2-" .. tostring(os.time())

for _, fn in ipairs({ "feed", "type_text" }) do
  assert(type(remuda[fn]) == "function", "remuda." .. fn .. " is gone")
end

-- bash, not the default shell: its readline enables bracketed paste for an
-- interactive session, which is what makes the multi-line case below mean
-- anything (a plain `sh` on most Linux images is dash, which does not).
remuda.new(name, { "bash" })

-- feed(name, steps) delivers bursts and a pause as one act — a single-line
-- burst, a short pause, then the submitting Return.
remuda.feed(name, {
  { burst = "echo $((3*3))-v2" },
  { pause = 0.05 },
  { burst = "\r" },
})
assert(type(remuda.capture(name)) == "string", "capture must return a string")

-- type_text(session, text) builds that same shape for multi-line text: a
-- bracketed paste so the embedded newline does not submit the first line
-- early, then Return.
remuda.type_text(name, "echo one-v2\necho two-v2")
assert(type(remuda.capture(name)) == "string", "capture must return a string")

-- A missing session raises, from both the raw binding and the function built
-- on it.
local ok = pcall(function()
  return remuda.feed("no-such-session-v2", { { burst = "x" } })
end)
assert(not ok, "a missing session must raise, not return nothing")

local ok2 = pcall(function()
  return remuda.type_text("no-such-session-v2", "x")
end)
assert(not ok2, "type_text over a missing session must raise too")

print("v2 ok")
