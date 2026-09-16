#!/usr/bin/env python3
"""A stub Matrix homeserver for the `packages/butler` tests. stdlib only —
no real Matrix homeserver is ever contacted by that test suite.

argv: <fixture_path> <get_log_path> <put_log_path> <send_status>

Binds 127.0.0.1:0 (OS-assigned) and prints the chosen port as the first
line of stdout, flushed, so the test harness can read it back.

GET /_matrix/client/v3/sync answers with the next line of `fixture_path`
(one JSON object per line, in order) each time it is called; every request
is also appended to `get_log_path` (full path + query string), which is how
a test proves what `since` value a later call carried. Once the fixture is
exhausted, it keeps answering with an empty room list and the SAME
`next_batch` it last used — deliberately not incrementing, so a caller that
is killed and restarted against a persisted `since` sees a stable token to
resume from, not a moving target.

PUT .../rooms/<id>/send/m.room.message/<txn> logs the request body to
`put_log_path` and answers with `send_status` (200 -> a fake event_id,
anything else -> a bare error body) — enough to test both the success and
failure paths of `remuda.process`'s `on_exit`.
"""
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

fixture_path, get_log_path, put_log_path, send_status = sys.argv[1:5]
send_status = int(send_status)

with open(fixture_path) as f:
    fixture = [json.loads(line) for line in f if line.strip()]

state = {"index": 0, "last_token": "stub-idle-0", "lock": threading.Lock()}


def append(path, line):
    with open(path, "a") as f:
        f.write(line + "\n")
        f.flush()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass  # keep test output quiet; the two log files are the real record

    def _reply(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path.split("?", 1)[0] != "/_matrix/client/v3/sync":
            self._reply(404, {"errcode": "M_NOT_FOUND"})
            return
        append(get_log_path, self.path)
        with state["lock"]:
            if state["index"] < len(fixture):
                body = fixture[state["index"]]
                state["index"] += 1
                state["last_token"] = body.get("next_batch", state["last_token"])
            else:
                body = {"rooms": {"join": {}}, "next_batch": state["last_token"]}
        self._reply(200, body)

    def do_PUT(self):
        if not re.match(r"^/_matrix/client/v3/rooms/[^/]+/send/m\.room\.message/", self.path):
            self._reply(404, {"errcode": "M_NOT_FOUND"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length).decode("utf-8", "replace")
        append(put_log_path, body)
        if send_status == 200:
            self._reply(200, {"event_id": "$stub-fake-event"})
        else:
            self._reply(send_status, {"errcode": "M_UNKNOWN", "error": "stub failure"})


def main():
    server = HTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
