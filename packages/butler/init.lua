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

curl -sf -X PUT \
  "$HOMESERVER/_matrix/client/v3/rooms/$ENC_ROOM/send/m.room.message/$TXN_ID" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "$BODY_JSON" >/dev/null
]==]

-- Exposed so tests can extract the exact embedded source without triggering
-- the side effects below (starting a real process/session needs real
-- config this test harness doesn't have, and shouldn't start one anyway).
remuda._butler_helper_src = HELPER_SRC
remuda._butler_reply_src = REPLY_SRC
if remuda._butler_test_mode then
  return
end

local token_path = os.getenv("REMUDA_BUTLER_TOKEN")
local config_path = os.getenv("REMUDA_BUTLER_CONFIG")
if not token_path or not config_path then
  error("remuda-butler needs REMUDA_BUTLER_TOKEN and REMUDA_BUTLER_CONFIG set", 0)
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
  .. "only calling the tool does."

local butler = remuda.new(nil, {
  "claude",
  "--mcp-config",
  mcp_config_path,
  "--strict-mcp-config",
  "--permission-mode",
  "auto",
  "--append-system-prompt",
  SYSTEM_PROMPT,
})

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
  remuda.send(butler, "[matrix · " .. sender .. "] " .. body)
end)

remuda.process{
  argv = {"python3", "-c", HELPER_SRC, token_path, config_path},
  on_line = "butler-matrix-line",
  on_exit = "butler-matrix-sync-exit",
}

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
