#!/usr/bin/env python3
"""Two-node GUI input smoke; isolated XDG dirs, never changes HOME."""
import json
import os
import pathlib
import signal
import socket
import struct
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
GUI = pathlib.Path(os.environ.get("REMUDA_GUI_ACCEPTANCE_WORKER", str(ROOT / "gui/target/debug/remuda-gui")))
REMUDA = pathlib.Path(os.environ["REMUDA_GUI_ACCEPTANCE_REMUDA"])
ABI = "--remuda-gui-worker-abi-1"


def main():
    daemons = []
    gui_test = None
    with tempfile.TemporaryDirectory(prefix="rgl-", dir="/private/tmp") as tmp:
        base = pathlib.Path(tmp)
        def node(name):
            root = base / name
            env = os.environ.copy()
            for key, folder in [("REMUDA_RUNTIME_DIR", "runtime"), ("XDG_RUNTIME_DIR", "runtime"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")]:
                path = root / folder
                path.mkdir(parents=True, exist_ok=True)
                env[key] = str(path)
            env["REMUDA_EXPERIMENTAL_SESSION_HOST"] = "0"
            daemon = subprocess.Popen([str(REMUDA), "daemon"], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
            daemons.append((daemon, env))
            deadline = time.monotonic() + 10
            while not (root / "runtime/remuda/default.sock").exists():
                if daemon.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("isolated daemon did not start")
                time.sleep(.02)
            run(env, ["cluster", "init", "--no-listen"])
            return root, env
        def run(env, args):
            return subprocess.run([str(REMUDA), *args], env=env, capture_output=True, timeout=12, check=True)
        def gui(env, args, expect=True):
            out = subprocess.run([str(GUI), *args], env=env, capture_output=True, timeout=12)
            if expect and out.returncode:
                raise RuntimeError(f"GUI worker failed: {out.stderr.decode(errors='replace')}")
            return out
        try:
            client_root, client_env = node("a")
            server_root, server_env = node("b")
            a_path = client_root / "state/remuda/cluster/authorized_nodes.json"
            b_path = server_root / "state/remuda/cluster/authorized_nodes.json"
            a = json.loads(a_path.read_text())["authorized_nodes"][0]
            b = json.loads(b_path.read_text())["authorized_nodes"][0]
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            b["endpoint"] = f"127.0.0.1:{port}"
            a_path.write_text(json.dumps({"authorized_nodes": [a, b]}))
            b_path.write_text(json.dumps({"authorized_nodes": [a, b]}))
            run(server_env, ["cluster", "listen", "--bind", f"127.0.0.1:{port}"])
            shell = "stty -echo; printf REMOTE_START; sleep 12; printf REMOTE_LATER; while IFS= read -r line; do printf \" INPUT:%s\" \"$line\"; done"
            lua = "remuda.new('proof', {'/bin/sh','-c'," + json.dumps(shell) + "})"
            run(server_env, ["-e", lua])
            rows = json.loads(gui(server_env, ["--internal-list-worker"]).stdout)
            instance = next(row["instance_id"] for row in rows if row["name"] == "proof")
            peer = b["node_fp"]
            test_env = client_env.copy()
            test_env.update({"REMUDA_GUI_TEST_PEER_FP": peer, "REMUDA_GUI_TEST_PEER_INSTANCE": instance, "REMUDA_GUI_ACCEPTANCE_WORKER": str(GUI)})
            test_binary = os.environ["REMUDA_GUI_PEER_TEST_BIN"]
            gui_test = subprocess.Popen([test_binary, "--ignored", "--exact", "tests::real_peer_live_view_reaches_gui"], env=test_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            nodes = json.loads(gui(client_env, ["--internal-peer-worker", peer]).stdout)
            assert any(row["name"] == "proof" and row["instance_id"] == instance for row in nodes)
            def sync(identity, since=0, fingerprint=peer, expect=True):
                args = ["--internal-preview-worker", "peer", fingerprint, "proof", identity, "--sync", str(since), ABI]
                return gui(client_env, args, expect)
            first = json.loads(sync(instance).stdout)["payload"]
            assert first["instance"] == instance and "REMOTE_START" in first["text"]
            assert "REMOTE_LATER" not in first["text"], "initial timing window was missed"
            deadline = time.monotonic() + 20
            while True:
                frame = json.loads(sync(instance, first["version"]).stdout)["payload"]
                if "REMOTE_LATER" in frame["text"]:
                    assert frame["version"] > first["version"]
                    break
                assert time.monotonic() < deadline, "new peer output was not observed"
            test_stdout, test_stderr = gui_test.communicate(timeout=20)
            assert gui_test.returncode == 0, f"GUI TestWindow failed: {test_stdout.decode(errors='replace')} {test_stderr.decode(errors='replace')}"
            assert b"running 1 test" in test_stdout and b"1 passed" in test_stdout, f"GUI TestWindow did not run the expected test: {test_stdout.decode(errors='replace')}"
            assert sync("wrong-instance", expect=False).returncode != 0
            assert sync(instance, fingerprint="not-admitted", expect=False).returncode != 0
            def reject_input(fingerprint, identity):
                command = [str(GUI), "--internal-input-worker", "peer", fingerprint, "proof", identity]
                data = b"SHOULD_NOT_REACH\r"
                out = subprocess.run(command, env=client_env, input=struct.pack(">I", len(data)) + data, capture_output=True, timeout=12)
                assert out.returncode != 0 and out.stdout == b"remuda-input-v2\n\x00", "invalid peer input was not refused"
            reject_input(peer, "wrong-instance")
            reject_input("not-admitted", instance)
            b["state"] = "revoked"
            a_path.write_text(json.dumps({"authorized_nodes": [a, b]}))
            assert sync(instance, expect=False).returncode != 0
            reject_input(peer, instance)
            local = json.loads(gui(server_env, ["--internal-preview-worker", "local", "", "proof", instance]).stdout)
            assert "SHOULD_NOT_REACH" not in local, "rejected input reached the test shell"
            print("PASS: GUI node/session clicks, live output and guarded input reached isolated peer; wrong instance, unadmitted and revoked peer refused for viewing and input")
        finally:
            if gui_test is not None and gui_test.poll() is None:
                try:
                    os.killpg(gui_test.pid, signal.SIGTERM)
                    gui_test.wait(timeout=3)
                except (OSError, subprocess.SubprocessError):
                    try:
                        os.killpg(gui_test.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    gui_test.wait()
            for daemon, env in reversed(daemons):
                try:
                    subprocess.run([str(REMUDA), "stop", "-f", "--yes"], env=env, capture_output=True, timeout=4)
                except (OSError, subprocess.SubprocessError):
                    pass
                if daemon.poll() is None:
                    try:
                        os.killpg(daemon.pid, signal.SIGTERM)
                        daemon.wait(timeout=3)
                    except (OSError, subprocess.SubprocessError):
                        try:
                            os.killpg(daemon.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        daemon.wait()


if __name__ == "__main__":
    main()
