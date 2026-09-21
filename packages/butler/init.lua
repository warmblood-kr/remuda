-- remuda-butler: runs one Claude Code session, optionally bridged to Matrix
-- and replying there via an MCP tool. See docs/design.md.

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

-- Claude calls statusLine commands with a JSON snapshot on stdin.  This
-- helper is deliberately the sole producer of Butler's telemetry: it emits a
-- fixed marker for people in the terminal and atomically publishes that exact
-- marker to a private file for `butler_status`.  Reading Claude's terminal
-- would make the latter depend on escape sequences and layout rather than the
-- protocol Claude itself supplies.
local STATUSLINE_SRC = [==[
import json
import os
import re
import sys

path = sys.argv[1]

def tag(value):
    if not isinstance(value, str) or not value:
        return "?"
    value = re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-")
    return value or "?"

def integer(value):
    return str(int(value)) if isinstance(value, (int, float)) else "?"

try:
    snapshot = json.load(sys.stdin)
except Exception:
    snapshot = {}

window = snapshot.get("context_window") or {}
used = window.get("total_input_tokens")
if not isinstance(used, (int, float)):
    current = window.get("current_usage") or {}
    parts = [current.get(key) for key in (
        "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")]
    parts = [part for part in parts if isinstance(part, (int, float))]
    used = sum(parts) if parts else None

model = snapshot.get("model") or {}
line = "MODEL:{model} CTX:{used} CTXWIN:{capacity} CTXPCT:{percent}".format(
    model=tag(model.get("display_name") or model.get("id")),
    used=integer(used),
    capacity=integer(window.get("context_window_size")),
    percent=integer(window.get("used_percentage")),
)

try:
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as out:
        out.write(line + "\n")
    os.replace(tmp, path)
except Exception:
    # A status line must never make Claude's UI fail merely because its
    # observer cannot write (for example a cleaned-up temporary directory).
    pass
print(line)
]==]

-- This is the one service session installed by the package, not an ordinary
-- user-created session. Its stable name is its public control surface:
-- `remuda send butler ...`, installer liveness checks, and restart recovery
-- must never depend on the directory that happened to start the daemon.
local function initial_butler_name()
  return "butler"
end

-- Exposed so tests can extract the exact embedded source without triggering
-- the side effects below (starting a real process/session needs real
-- config this test harness doesn't have, and shouldn't start one anyway).
remuda._butler_helper_src = HELPER_SRC
remuda._butler_reply_src = REPLY_SRC
remuda._butler_statusline_src = STATUSLINE_SRC
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

-- Fails loudly when Matrix has been configured -- naming the exact path it
-- tried and mentioning the override -- rather than silently proceeding with
-- a path that doesn't resolve to a real file.
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

local function file_exists(path)
  if not path then
    return false
  end
  local f = io.open(path, "r")
  if not f then
    return false
  end
  f:close()
  return true
end

-- Matrix is an optional Butler integration. An explicit override means its
-- caller intended to enable it, and either conventional credential file
-- means a half-configured relay should still fail loudly. With neither,
-- Butler remains a local Claude-session manager and simply omits the relay.
local token_override = os.getenv("REMUDA_BUTLER_TOKEN")
local config_override = os.getenv("REMUDA_BUTLER_CONFIG")
local config_home = default_config_home()
local default_token_path = config_home and config_home .. "/remuda/butler/token"
local default_config_path = config_home and config_home .. "/remuda/butler/config"
local matrix_requested = (token_override and token_override ~= "")
  or (config_override and config_override ~= "")
  or file_exists(default_token_path)
  or file_exists(default_config_path)

local token_path = nil
local config_path = nil
if matrix_requested then
  token_path = resolve_path("REMUDA_BUTLER_TOKEN", "token", "token file")
  config_path = resolve_path("REMUDA_BUTLER_CONFIG", "config", "config file")
end

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
-- Matrix-enabled installs keep this beside their relay configuration. The
-- local-only mode has no configuration directory to rely on, so use a private
-- temporary filename for the same short-lived Claude MCP configuration.
local mcp_config_path = config_path and (config_path .. ".mcp.json") or (os.tmpname() .. ".mcp.json")
local status_path = config_path and (config_path .. ".status") or (os.tmpname() .. ".status")
local status_helper_path = status_path .. ".py"
local settings_path = status_path .. ".settings.json"

-- The helper is embedded with the package so a local `remuda` binary is
-- self-contained: no dotfile, PATH helper, or repository checkout has to
-- survive after it starts Claude.  These paths are generated once per Butler
-- registration and reused by respawns through BUTLER_ARGV below.
local status_helper = assert(io.open(status_helper_path, "w"))
status_helper:write(STATUSLINE_SRC)
status_helper:close()
local function shell_quote(s)
  return "'" .. s:gsub("'", "'\\\"'\\\"'") .. "'"
end
local function json_quote(s)
  return '"' .. s:gsub('\\', '\\\\'):gsub('"', '\\"') .. '"'
end
local settings = assert(io.open(settings_path, "w"))
settings:write('{"statusLine":{"type":"command","command":'
  .. json_quote("python3 " .. shell_quote(status_helper_path) .. " " .. shell_quote(status_path))
  .. ',"refreshInterval":2}}')
settings:close()
remuda._butler_status_path = status_path

remuda.tool{
  name = "butler_status",
  about = "Read Butler's latest Claude Code status-line telemetry: model, context tokens, window, and percentage.",
  run = function()
    local f = io.open(remuda._butler_status_path or "", "r")
    if not f then
      return "MODEL:? CTX:? CTXWIN:? CTXPCT:? (no status reading yet)"
    end
    local line = f:read("*l")
    f:close()
    -- The helper owns this file.  Refuse a malformed or externally replaced
    -- record instead of presenting arbitrary file contents as Claude status.
    if not line or not line:match("^MODEL:[A-Za-z0-9_.%-?]+ CTX:[0-9?]+ CTXWIN:[0-9?]+ CTXPCT:[0-9?]+$") then
      error("butler status record is malformed", 0)
    end
    return line
  end,
}

-- A small, cooperative post office for every agent Butler launches.  This is
-- deliberately live-image state, like Emacs: callers may inspect or extend it
-- through `run_script`.  `caller.capability` is attribution supplied by that
-- session's MCP child, not an access-control boundary.
remuda._butler_bus = remuda._butler_bus or { agents = {}, tokens = {}, inboxes = {}, next = 0 }
local bus = remuda._butler_bus
local function next_token(name)
  bus.next = bus.next + 1
  return name .. "-" .. os.time() .. "-" .. bus.next
end
local function caller_name(caller)
  local token = caller and caller.capability
  return (token and bus.tokens[token]) or "outside"
end
local function mailbox(name)
  bus.inboxes[name] = bus.inboxes[name] or {}
  return bus.inboxes[name]
end
local function agent_mcp_json(token)
  local env = '"REMUDA_BUTLER_SESSION_TOKEN":"' .. token .. '"'
  if runtime_dir then env = env .. ',"REMUDA_RUNTIME_DIR":"' .. runtime_dir .. '"' end
  return '{"mcpServers":{"remuda":{"command":"remuda","args":["-s","'
    .. server .. '","mcp"],"env":{' .. env .. '}}}}'
end
local function agent_mcp_path(name, token)
  local path = os.tmpname() .. "." .. name .. ".mcp.json"
  local f = assert(io.open(path, "w"))
  f:write(agent_mcp_json(token))
  f:close()
  return path
end
local function codex_mcp_flags(token)
  local env = 'REMUDA_BUTLER_SESSION_TOKEN="' .. token .. '"'
  if runtime_dir then env = env .. ',REMUDA_RUNTIME_DIR="' .. runtime_dir .. '"' end
  return {
    "-c", 'mcp_servers.remuda.command="remuda"',
    "-c", 'mcp_servers.remuda.args=["-s","' .. server .. '","mcp"]',
    "-c", "mcp_servers.remuda.env={" .. env .. "}",
  }
end
-- Adapters build argv; Butler owns lifecycle and mail.
local AGENT_BUILDERS = {}
AGENT_BUILDERS.claude = function(spec)
  local argv = { "claude" }
  if spec.settings_path then argv[#argv + 1] = "--settings"; argv[#argv + 1] = spec.settings_path end
  argv[#argv + 1] = "--mcp-config"; argv[#argv + 1] = spec.mcp_config_path or agent_mcp_path(spec.name, spec.token)
  argv[#argv + 1] = "--strict-mcp-config"
  argv[#argv + 1] = "--permission-mode"; argv[#argv + 1] = "auto"
  argv[#argv + 1] = "--append-system-prompt"
  argv[#argv + 1] = spec.system_prompt
    or "This session is managed by Remuda Butler. The remuda butler CLI is available for coordination."
  if spec.model and spec.model ~= "" then argv[#argv + 1] = "--model"; argv[#argv + 1] = spec.model end
  return argv
end
AGENT_BUILDERS.codex = function(spec)
  local argv = { "codex" }
  for _, flag in ipairs(codex_mcp_flags(spec.token)) do argv[#argv + 1] = flag end
  argv[#argv + 1] = "--approve-for-me"
  if spec.model and spec.model ~= "" then argv[#argv + 1] = "--model"; argv[#argv + 1] = spec.model end
  return argv
end
local function build_agent_argv(kind, spec)
  local builder = AGENT_BUILDERS[kind]
  if not builder then error("unknown agent kind: " .. tostring(kind), 0) end
  return builder(spec)
end
local function launch_agent(kind, requested_name, cwd, model)
  local name = requested_name or kind
  local token = next_token(name)
  local argv = build_agent_argv(kind, { name = name, token = token, model = model })
  local actual = remuda.new(name, argv, cwd)
  bus.tokens[token] = actual
  bus.agents[actual] = { kind = kind, token = token }
  mailbox(actual)
  return actual
end

-- Shell-facing doors into the same deliberately mutable bus.  These are not
-- capability checks: Butler is a workshop, and the `from` name is simply the
-- attribution a human (or an agent using the CLI) chose to leave on a note.
-- Keeping them on `remuda` also makes the post office pleasant to explore from
-- a REPL without having to know this chunk's private locals.
function remuda._butler_launch(kind, name)
  return launch_agent(kind, name)
end
function remuda._butler_send(from, to, text)
  if not bus.agents[to] then error("no Butler agent named " .. tostring(to), 0) end
  bus.next = bus.next + 1
  local id = "message-" .. bus.next
  mailbox(to)[#mailbox(to) + 1] = { id = id, from = from or "outside", body = text }
  return "queued " .. id .. " for " .. to
end
function remuda._butler_inbox(name)
  if not bus.agents[name] then error("no Butler agent named " .. tostring(name), 0) end
  local messages = mailbox(name)
  if #messages == 0 then return "inbox empty" end
  local out = {}
  for _, message in ipairs(messages) do
    out[#out + 1] = "[" .. message.id .. " from " .. message.from .. "] " .. message.body
  end
  bus.inboxes[name] = {}
  return table.concat(out, "\n")
end
function remuda._butler_sessions()
  local out = {}
  for name, agent in pairs(bus.agents) do out[#out + 1] = name .. "\t" .. agent.kind end
  table.sort(out)
  return #out == 0 and "no Butler agents" or table.concat(out, "\n")
end

remuda.tool{
  name = "butler_launch",
  about = "Launch a Claude Code or Codex agent with this Butler's shared MCP mailbox.",
  args = { kind = "Agent kind: claude or codex.", name = "Optional session name.", cwd = "Optional working directory.", model = "Optional model override." },
  needs = { "kind" },
  run = function(a)
    return "launched " .. launch_agent(a.kind, a.name, a.cwd, a.model)
  end,
}
remuda.tool{
  name = "butler_send",
  about = "Queue a message for another Butler agent without typing its body into that agent's terminal.",
  args = { to = "Recipient session name.", text = "Message body." },
  needs = { "to", "text" },
  run = function(a, caller)
    return remuda._butler_send(caller_name(caller), a.to, a.text)
  end,
}
remuda.tool{
  name = "butler_inbox",
  about = "Drain this agent's Butler inbox and return its queued messages in arrival order.",
  run = function(_, caller)
    return remuda._butler_inbox(caller_name(caller))
  end,
}
remuda.tool{
  name = "butler_reply",
  about = "Reply to a received Butler message by its message id; the reply goes to its recorded sender.",
  args = { to = "Recipient session name.", text = "Reply body." },
  needs = { "to", "text" },
  run = function(a, caller)
    if not bus.agents[a.to] then error("no Butler agent named " .. a.to, 0) end
    bus.next = bus.next + 1
    mailbox(a.to)[#mailbox(a.to) + 1] = { id = "message-" .. bus.next, from = caller_name(caller), body = a.text }
    return "reply queued for " .. a.to
  end,
}
remuda.tool{
  name = "butler_sessions",
  about = "List Butler-managed Claude Code and Codex agent sessions and their adapter kinds.",
  run = function()
    return remuda._butler_sessions()
  end,
}

local butler_token = next_token("butler")
bus.tokens[butler_token] = "butler"
bus.agents.butler = { kind = os.getenv("REMUDA_BUTLER_KIND") or "claude", token = butler_token }
mailbox("butler")
local mcp_file = io.open(mcp_config_path, "w")
mcp_file:write(agent_mcp_json(butler_token))
mcp_file:close()

-- Without this, the session has no reason to ever call `matrix_reply`: a
-- line arriving on its terminal is not by itself an instruction to use a
-- specific tool. This is what turns "text showed up" into "the tool got
-- called" for an otherwise-unprompted incoming message.
local SYSTEM_PROMPT = "Early in this session, call remuda._butler_register_compaction_schedule() "
  .. "via the run_script MCP tool, once, to enable this butler's own periodic "
  .. "context-compaction upkeep."
if token_path then
  SYSTEM_PROMPT = "You are bridged into one Matrix room via remuda. "
    .. "Every line you receive here that starts with \"[matrix · \" is a "
    .. "message from that room, not from the person running this terminal. "
    .. "Reply to it by calling the matrix_reply MCP tool with your response "
    .. "text -- printing a reply in this terminal does not send it anywhere; "
    .. "only calling the tool does. "
    .. SYSTEM_PROMPT
end

-- Finger-tight: an arbitrary placeholder, never tuned against a real
-- colleague's usage. Tightening step: revisit once this has run on a real
-- machine for a real "몇 날" and someone has an opinion about the cadence.
-- remuda._butler_compaction_interval lets a test override it (same idiom as
-- every other remuda._butler_* test hook in this file).
local COMPACTION_CHECK_INTERVAL = remuda._butler_compaction_interval or 30 * 60

-- remuda._butler_compaction_trace_path lets a test redirect the append-only
-- trace below to a throwaway tempfile instead of the real config dir (same
-- idiom as remuda._butler_compaction_interval just above). nil in
-- production falls back to the real default, matching the token/config
-- path convention already used by default_config_home() above.
-- Confirmed at the source level (lua-src's vendored loslib.c, the "lua54"
-- feature this crate builds with): a leading "!" in os.date's format
-- routes through l_gmtime, not l_localtime -- so "!%Y-%m-%dT%H:%M:%SZ"
-- below is genuinely UTC, not merely assumed to be.
local function _butler_trace(event, detail)
  pcall(function()
    local path = remuda._butler_compaction_trace_path
      or (os.getenv("XDG_CONFIG_HOME") or (os.getenv("HOME") .. "/.config"))
        .. "/remuda/compaction-trace.log"
    local f = io.open(path, "a")
    if not f then
      -- Stock Lua's io has no mkdir; a one-time `mkdir -p` on first-open
      -- failure is smaller than documenting "the directory must already
      -- exist" as a precondition every caller (including every test) has
      -- to remember to satisfy.
      os.execute('mkdir -p "' .. path:match("^(.*)/[^/]+$") .. '"')
      f = io.open(path, "a")
    end
    if not f then
      return
    end
    f:write(os.date("!%Y-%m-%dT%H:%M:%SZ") .. "\t" .. event .. "\t" .. (detail or "") .. "\n")
    f:close()
  end)
end

local butler_kind = os.getenv("REMUDA_BUTLER_KIND") or "claude"
local BUTLER_ARGV = remuda._butler_argv
if not BUTLER_ARGV then
  BUTLER_ARGV = build_agent_argv(butler_kind, {
    name = "butler", token = butler_token, mcp_config_path = mcp_config_path, settings_path = settings_path,
    system_prompt = SYSTEM_PROMPT,
  })
end

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
  _butler_trace("registered")
  remuda._butler_compaction_schedule = remuda.schedule({
    name = "butler-compaction",
    every = COMPACTION_CHECK_INTERVAL,
    run = function()
      -- `context_left` is unimplemented (tools.lua:362-366, canon says "지금
      -- 안 만든다") -- `is_busy` (idle-time heuristic, never a real token
      -- count) is the proxy the canon names instead: only ever nudge
      -- compaction while the session looks idle, never mid-task.
      if butler_name and remuda.session(butler_name).is_busy == false then
        local ok, err = pcall(remuda.send, butler_name, "/compact")
        if ok then
          _butler_trace("sent")
        else
          _butler_trace("error", tostring(err))
        end
        -- Same "type it, wait, then submit" hand-off the Matrix relay below
        -- already uses -- `remuda.send`'s text+Enter lands as one write,
        -- which this TUI reads as paste-in-progress rather than a distinct
        -- Enter, so a separately-timed bare Enter confirms it.
        remuda.process({ argv = { "sleep", "2" }, on_exit = "butler-compaction-submit" })
      else
        _butler_trace("skipped_busy")
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

if token_path and not remuda._butler_skip_relay then
  remuda.process{
    argv = {"python3", "-c", HELPER_SRC, token_path, config_path},
    on_line = "butler-matrix-line",
    on_exit = "butler-matrix-sync-exit",
  }
end

if token_path then
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
end
