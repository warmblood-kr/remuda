#!/usr/bin/env python3
"""Check an actual v1 parent rejects the v2 worker before writing input."""
import os
import pathlib
import signal
import subprocess
import tempfile
import time

PROXY = r'''#!/usr/bin/python3
import os, subprocess, sys
capture = os.environ["TASK_BRIDGE_CAPTURE"]
open(capture + ".started", "wb").write(b"started")
child = subprocess.Popen([os.environ["TASK_NEW_GUI"], *sys.argv[1:]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
open(capture + ".worker-pid", "w").write(str(child.pid))
def forward_input():
    try:
        while True:
            data = os.read(0, 4096)
            if not data: break
            with open(capture, "ab") as f: f.write(data)
            child.stdin.write(data)
            child.stdin.flush()
    except (BrokenPipeError, OSError):
        pass
    finally:
        child.stdin.close()
observer = os.fork()
if observer == 0:
    forward_input()
    open(capture + ".drained", "wb").write(b"drained")
    os._exit(0)
while True:
    data = os.read(child.stdout.fileno(), 4096)
    if not data: break
    if not os.path.exists(capture + ".greeting"):
        open(capture + ".greeting", "wb").write(data)
    try:
        os.write(1, data)
    except BrokenPipeError:
        break
child.wait(timeout=10)
open(capture + ".finished", "wb").write(b"finished")
'''


def main():
    old_bridge = os.environ["REMUDA_GUI_OLD_BRIDGE"]
    new_gui = os.environ["REMUDA_GUI_NEW_BINARY"]
    with tempfile.TemporaryDirectory(prefix="rgt-", dir="/private/tmp") as temp:
        root = pathlib.Path(temp)
        proxy = root / "proxy"
        proxy.write_text(PROXY)
        proxy.chmod(0o700)
        env = os.environ.copy()
        env.update({"REMUDA_GUI_ACCEPTANCE_BIN": str(proxy), "TASK_NEW_GUI": new_gui, "TASK_BRIDGE_CAPTURE": str(root / "capture")})
        for key, folder in [("REMUDA_RUNTIME_DIR", "runtime"), ("XDG_RUNTIME_DIR", "runtime"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")]:
            path = root / folder
            path.mkdir(exist_ok=True)
            env[key] = str(path)
        parent = subprocess.Popen([old_bridge, "local", "", "synthetic-instance"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        try:
            out, err = parent.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(parent.pid, signal.SIGKILL)
            parent.communicate()
            raise RuntimeError("old-parent transition check timed out")
        assert parent.returncode != 0, "old parent unexpectedly accepted the new worker"
        assert b"NotAcknowledged" in err and b"Applied" in err, "old parent failed for an unrelated reason"
        assert (root / "capture.started").read_bytes() == b"started", "proxy was not invoked"
        assert (root / "capture.greeting").read_bytes().startswith(b"remuda-input-v2\n"), "v2 worker did not greet the old parent"
        deadline = time.monotonic() + 5
        while not (root / "capture.drained").exists() and time.monotonic() < deadline:
            time.sleep(.05)
        assert (root / "capture.drained").read_bytes() == b"drained", "input observer did not drain to EOF"
        assert not (root / "capture").exists() or (root / "capture").read_bytes() == b"", "old parent sent input bytes"
        worker_pid = int((root / "capture.worker-pid").read_text())
        deadline = time.monotonic() + 12
        while True:
            try:
                os.kill(worker_pid, 0)
            except ProcessLookupError:
                break
            assert time.monotonic() < deadline, "v2 worker remained after old parent closed"
            time.sleep(.05)
        print("PASS: actual v1 parent rejected v2 worker before sending input; worker exited")


if __name__ == "__main__":
    main()
