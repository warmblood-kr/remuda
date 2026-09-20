-- remuda-butler: runs one Claude Code session fed by Matrix messages,
-- replying via an MCP tool. See docs/design.md.

local HELPER_SRC = [==[
import json
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

TOKEN_PATH, CONFIG_PATH = sys.argv[1], sys.argv[2]
TOKEN = Path(TOKEN_PATH).read_text().strip()
_lines = Path(CONFIG_PATH).read_text().splitlines()
HOMESERVER, ROOM_ID, SELF_MXID = _lines[0].strip(), _lines[1].strip(), _lines[2].strip()
ALLOWED_SENDERS = set()
if len(_lines) > 3 and _lines[3].strip():
    ALLOWED_SENDERS = {s.strip() for s in _lines[3].split(",") if s.strip()}

SYNC_TIMEOUT_MS = 30000
SINCE_FILE = Path(CONFIG_PATH + ".since")


def load_since():
    if SINCE_FILE.exists():
        try:
            return json.loads(SINCE_FILE.read_text()).get("since")
        except (ValueError, AttributeError):
            return None
    return None


def save_since(token):
    tmp = SINCE_FILE.with_name(SINCE_FILE.name + ".tmp")
    tmp.write_text(json.dumps({"since": token}))
    tmp.replace(SINCE_FILE)


def matrix_get(path, params=None):
    url = HOMESERVER + path
    if params:
        url += "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + TOKEN})
    with urllib.request.urlopen(req, timeout=(SYNC_TIMEOUT_MS / 1000) + 10) as resp:
        return json.loads(resp.read())


def emit(sender, body):
    # Backslash escaped FIRST, then newline, so the result can never itself
    # contain a raw newline: remuda.process's pipe is strictly line-oriented,
    # so one Matrix event must become exactly one physical output line.
    escaped = body.replace("\\", "\\\\").replace("\n", "\\n")
    sys.stdout.write(sender + "\t" + escaped + "\n")
    sys.stdout.flush()


def handle_room(room):
    for ev in room.get("timeline", {}).get("events", []):
        if ev.get("type") != "m.room.message":
            continue
        sender = ev.get("sender")
        if sender == SELF_MXID:
            continue
        # Sender allowlist: only a configured human account may reach the
        # session. `--permission-mode auto` gives that session shell access
        # with no per-call confirmation, so anyone else in the room must
        # never be able to feed it input.
        if sender not in ALLOWED_SENDERS:
            continue
        content = ev.get("content", {})
        if content.get("msgtype") not in ("m.text", "m.notice", "m.emote"):
            continue
        emit(sender, content.get("body", ""))


def main():
    since = load_since()
    if since is None:
        # First run: establish a baseline without replaying room history.
        resp = matrix_get("/_matrix/client/v3/sync", {"timeout": "0"})
        since = resp["next_batch"]
        save_since(since)

    while True:
        try:
            resp = matrix_get(
                "/_matrix/client/v3/sync",
                {"since": since, "timeout": str(SYNC_TIMEOUT_MS)},
            )
        # Deliberately broad: a relay must outlive every transport failure,
        # not just urllib.error.URLError (a killed connection mid-request
        # raises ConnectionResetError/RemoteDisconnected, which is not one).
        except Exception:
            time.sleep(5)
            continue

        # Allowlist: only ever look at the one configured room. Never
        # iterate any other key of resp["rooms"]["join"].
        room = resp.get("rooms", {}).get("join", {}).get(ROOM_ID)
        if room:
            handle_room(room)

        since = resp["next_batch"]
        save_since(since)


if __name__ == "__main__":
    main()
]==]

local REPLY_SRC = [==[
set -euo pipefail

TOKEN="$(cat "$1")"
HOMESERVER="$(sed -n '1p' "$2")"
ROOM_ID="$(sed -n '2p' "$2")"
TEXT="$3"

TXN_ID="remuda-butler-$(date +%s%N)"
BODY_JSON="$(python3 -c 'import json,sys; print(json.dumps({"msgtype":"m.text","body":sys.argv[1]}))' "$TEXT")"
ENC_ROOM="$(python3 -c 'import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=""))' "$ROOM_ID")"

# The Authorization header carries the bearer token; passing it via -H would
# put the token in this process's own argv, visible to any other user via
# `ps`. -K - reads curl's config (here, just the one header) from stdin
# instead, which never appears in argv.
printf 'header = "Authorization: Bearer %s"\n' "$TOKEN" | curl -sf -K - -X PUT \
  "$HOMESERVER/_matrix/client/v3/rooms/$ENC_ROOM/send/m.room.message/$TXN_ID" \
  -H "Content-Type: application/json" \
  -d "$BODY_JSON" >/dev/null
]==]

-- No cwd binding exists on the `remuda` table, so PWD (set by the shell that
-- started the daemon) is the only directory this Lua code can see. A root
-- or absent PWD has no usable basename, so it falls back to "butler".
local function initial_butler_name()
  local pwd = os.getenv("PWD")
  local base = pwd and pwd:gsub("/+$", ""):match("([^/]+)$")
  return base or "butler"
end

-- Exposed so tests can extract the exact embedded source without triggering
-- the side effects below (starting a real process/session needs real
-- config this test harness doesn't have, and shouldn't start one anyway).
remuda._butler_helper_src = HELPER_SRC
remuda._butler_reply_src = REPLY_SRC
remuda._butler_initial_name = initial_butler_name()
if remuda._butler_test_mode then
  return
end

-- `os.getenv` here reads the *daemon's own* environment, fixed forever at
-- whichever moment first birthed that daemon (see docs/install-butler.sh's
-- ceiling comment) -- so a butler-specific env var as the primary source
-- means anything else that races to auto-start a daemon first leaves no
-- later `exec butler` call able to inject a corrected value (that's the bug
-- this resolves). `HOME` (or `XDG_CONFIG_HOME`) is present in essentially
-- every process's environment regardless of what happened to birth the
-- daemon (the known exception: a systemd *system* unit with `User=` set but
-- no PAM session, or a process launched via `env -i`) -- `resolve_path`
-- below fails loudly by name when it's genuinely absent, rather than
-- guessing. `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG` remain a supported
-- override, checked first, for a caller who wants a different location --
-- this is also what keeps every existing test that sets them via
-- `Daemon::spawn_with_env` unchanged. Mirrors install-butler.sh's own
-- `${XDG_CONFIG_HOME:-$HOME/.config}/remuda/butler/{token,config}` exactly,
-- kept in sync with install-butler.sh's own default by
-- scripts/check-butler-path-convention.py, which fails if the two diverge.
local function default_config_home()
  local xdg = os.getenv("XDG_CONFIG_HOME")
  if xdg and xdg ~= "" then
    return xdg
  end
  local home = os.getenv("HOME")
  if not home or home == "" then
    return nil
  end
  return home .. "/.config"
end

-- Fails loudly the same way the installer already does -- naming the exact
-- path it tried and mentioning the override -- rather than silently
-- proceeding with a path that doesn't resolve to a real file.
local function resolve_path(override_env, filename, what)
  local path = os.getenv(override_env)
  if not path or path == "" then
    local config_home = default_config_home()
    if not config_home then
      error(
        "remuda-butler: HOME is not set and " .. override_env .. " was not "
          .. "given -- cannot locate the " .. what,
        0
      )
    end
    path = config_home .. "/remuda/butler/" .. filename
  end
  local f = io.open(path, "r")
  if not f then
    error(
      "remuda-butler: no " .. what .. " at " .. path .. " -- create it, or "
        .. "set " .. override_env .. " to override",
      0
    )
  end
  f:close()
  return path
end

local token_path = resolve_path("REMUDA_BUTLER_TOKEN", "token", "token file")
local config_path = resolve_path("REMUDA_BUTLER_CONFIG", "config", "config file")

-- The session needs an `--mcp-config` pointing back at this same daemon, or
-- it has no way to reach `matrix_reply` at all — a bare `remuda.new(nil,
-- {"claude"})` starts a session with no MCP server configured. `claude`
-- only accepts that config as a file path, never inline JSON, so this is a
-- legitimate, unavoidable use of `io`/`os` (unlike embedding a companion
-- script, which argv already handles without touching a file).
-- `REMUDA_BUTLER_SERVER` names the running daemon's own `-s <name>`, so the
-- spawned `remuda ... mcp` reaches the exact instance running this code,
-- not some other "default" one; it defaults to "default" to match the CLI's
-- own default when no `-s` flag was given. `--permission-mode auto` skips
-- the second, tool-call permission dialog entirely (its default is "Yes",
-- the opposite framing from the trust dialog's "No, exit" — measured in
-- native/tests/claude_session.rs) since nothing here can answer it.
local server = os.getenv("REMUDA_BUTLER_SERVER") or "default"
local runtime_dir = os.getenv("REMUDA_RUNTIME_DIR")
local mcp_config_path = config_path .. ".mcp.json"
local mcp_env = ""
if runtime_dir then
  mcp_env = ',"env":{"REMUDA_RUNTIME_DIR":"' .. runtime_dir .. '"}'
end
local mcp_file = io.open(mcp_config_path, "w")
mcp_file:write(
  '{"mcpServers":{"remuda":{"command":"remuda","args":["-s","'
    .. server
    .. '","mcp"]'
    .. mcp_env
    .. "}}}"
)
mcp_file:close()

-- Without this, the session has no reason to ever call `matrix_reply`: a
-- line arriving on its terminal is not by itself an instruction to use a
-- specific tool. This is what turns "text showed up" into "the tool got
-- called" for an otherwise-unprompted incoming message.
local SYSTEM_PROMPT = "You are bridged into one Matrix room via remuda. "
  .. "Every line you receive here that starts with \"[matrix · \" is a "
  .. "message from that room, not from the person running this terminal. "
  .. "Reply to it by calling the matrix_reply MCP tool with your response "
  .. "text -- printing a reply in this terminal does not send it anywhere; "
  .. "only calling the tool does. "
  .. "Early in this session, call remuda._butler_register_compaction_schedule() "
  .. "via the run_script MCP tool, once, to enable this butler's own periodic "
  .. "context-compaction upkeep."

-- Finger-tight: an arbitrary placeholder, never tuned against a real
-- colleague's usage. Tightening step: revisit once this has run on a real
-- machine for a real "몇 날" and someone has an opinion about the cadence.
-- remuda._butler_compaction_interval lets a test override it (same idiom as
-- every other remuda._butler_* test hook in this file).
local COMPACTION_CHECK_INTERVAL = remuda._butler_compaction_interval or 30 * 60

local BUTLER_ARGV = remuda._butler_argv or {
  "claude",
  "--mcp-config",
  mcp_config_path,
  "--strict-mcp-config",
  "--permission-mode",
  "auto",
  "--allowedTools",
  "mcp__remuda__run_script",
  "--append-system-prompt",
  SYSTEM_PROMPT,
}

-- Reused both for the initial launch and every respawn, so the watchdog
-- below can never drift from what a fresh start would have done. Keeps the
-- name across respawns by feeding the previous result back in as the name.
local butler_name = nil
local function launch_butler()
  butler_name = remuda.new(butler_name or remuda._butler_initial_name, BUTLER_ARGV)
end
launch_butler()

-- Reused across every re-`exec` and every later call from the launched
-- session's own run_script -- a plain Lua local would NOT survive either
-- (each `exec butler` is a fresh chunk with fresh locals; a later run_script
-- call is a wholly separate Eval). `remuda._butler_compaction_schedule`
-- lives on the persistent `remuda` table, so only a slot on that same table
-- can hold "the one we already registered" across calls -- same reasoning
-- as `remuda._butler_argv` and friends, just read back instead of only
-- written. `run_script` needs no separate registration step to reach this:
-- it evals arbitrary Lua against the daemon's live globals (`mcp.rs`'s
-- `run_script => Request::Eval{code}`), so a plain function assigned onto
-- `remuda` is already callable by name from a later run_script call, exactly
-- like `remuda._butler_initial_name` already is.
function remuda._butler_register_compaction_schedule()
  if remuda._butler_compaction_schedule then
    remuda.cancel(remuda._butler_compaction_schedule)
  end
  remuda._butler_compaction_schedule = remuda.schedule({
    name = "butler-compaction",
    every = COMPACTION_CHECK_INTERVAL,
    run = function()
      -- `context_left` is unimplemented (tools.lua:362-366, canon says "지금
      -- 안 만든다") -- `is_busy` (idle-time heuristic, never a real token
      -- count) is the proxy the canon names instead: only ever nudge
      -- compaction while the session looks idle, never mid-task.
      if butler_name and remuda.session(butler_name).is_busy == false then
        remuda.send(butler_name, "/compact")
        -- Same "type it, wait, then submit" hand-off the Matrix relay below
        -- already uses -- `remuda.send`'s text+Enter lands as one write,
        -- which this TUI reads as paste-in-progress rather than a distinct
        -- Enter, so a separately-timed bare Enter confirms it.
        remuda.process({ argv = { "sleep", "2" }, on_exit = "butler-compaction-submit" })
      end
    end,
  })
  return remuda._butler_compaction_schedule
end

-- `exec butler` re-running this file in the same daemon image would
-- otherwise double this hook (see docs/design.md's augroup note) --
-- clearing the group first keeps exactly one watchdog alive.
remuda.clear_hooks({ group = "butler" })
remuda.on("session_exited", function(name)
  if name == butler_name then
    launch_butler()
  end
end, { group = "butler" })

remuda.on("butler-compaction-submit", function()
  remuda.send(butler_name, "")
end, { group = "butler" })

remuda.on("butler-matrix-line", function(line)
  local sender, body = line:match("^([^\t]*)\t(.*)$")
  if not body then
    return
  end
  -- One pass, not two sequential gsubs: a two-pass unescape would let an
  -- escaped backslash immediately followed by a literal "n" in the original
  -- text (e.g. someone pasting `\n` as text, not a newline) get misread as
  -- a newline escape on the first pass. Matching `\\(.)` and deciding per
  -- match consumes each escape atomically, left to right.
  body = body:gsub("\\(.)", function(c)
    return c == "n" and "\n" or c
  end)
  remuda.send(butler_name, "[matrix · " .. sender .. "] " .. body)
  -- `remuda.send`'s text+Enter lands as one write, and this TUI reads a
  -- burst of printable text immediately followed by \r as paste-in-progress,
  -- not "text, then a distinct Enter" (measured in
  -- native/tests/claude_session.rs's `send_and_submit`) -- so the line above
  -- sits typed but unsubmitted until a separately-timed, empty `send` (a
  -- bare Enter, its own write) confirms it. `remuda.sleep` would block the
  -- Image's whole job queue for the delay; a `remuda.process` running `sleep`
  -- gets the same delay without blocking anything else queued behind it.
  -- Enter on an already-submitted empty box is a no-op, so this is safe even
  -- if two lines arrive close together.
  remuda.process{
    argv = {"sleep", "2"},
    on_exit = "butler-matrix-submit",
  }
end)

remuda.on("butler-matrix-submit", function()
  remuda.send(butler_name, "")
end)

if not remuda._butler_skip_relay then
  remuda.process{
    argv = {"python3", "-c", HELPER_SRC, token_path, config_path},
    on_line = "butler-matrix-line",
    on_exit = "butler-matrix-sync-exit",
  }
end

remuda.tool{
  name = "matrix_reply",
  about = "Send a text reply into the bridged Matrix room. Fire-and-forget: "
    .. "returns once the send is queued, not once it is delivered — check "
    .. "for delivery failure separately if that matters.",
  args = { text = "The reply text to send." },
  needs = { "text" },
  run = function(a)
    remuda.process{
      argv = {"bash", "-c", REPLY_SRC, "_", token_path, config_path, a.text},
      on_exit = "butler-matrix-reply-exit",
    }
    return "queued"
  end,
}
