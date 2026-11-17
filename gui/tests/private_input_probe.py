#!/usr/bin/env python3
"""Private daemon + TestWindow line input, attached terminal and paste acceptance."""
import os
import pathlib
import signal
import subprocess
import tempfile
import time

cli = os.environ['REMUDA_GUI_ACCEPTANCE_REMUDA']
worker = os.environ['REMUDA_GUI_ACCEPTANCE_WORKER']
test_bin = os.environ['REMUDA_GUI_DOCUMENT_TEST_BIN']
with tempfile.TemporaryDirectory(prefix='rgdoc-', dir='/private/tmp') as temp:
    root = pathlib.Path(temp)
    env = os.environ.copy()
    for key, part in [('REMUDA_RUNTIME_DIR', 'runtime'), ('XDG_RUNTIME_DIR', 'runtime'), ('XDG_STATE_HOME', 'state'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')]:
        path = root / part
        path.mkdir(exist_ok=True)
        env[key] = str(path)
    env['REMUDA_EXPERIMENTAL_SESSION_HOST'] = '0'
    env['REMUDA_GUI_ACCEPTANCE_WORKER'] = worker
    env['REMUDA_GUI_ACCEPTANCE_INPUT'] = '1'
    def run(args, data=None):
        return subprocess.run([cli, *args], env=env, input=data, capture_output=True, check=True, timeout=12)
    daemon = subprocess.Popen([cli, 'daemon'], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
    try:
        until = time.monotonic() + 10
        while not (root / 'runtime/remuda/default.sock').exists():
            if daemon.poll() is not None or time.monotonic() > until:
                raise RuntimeError('private daemon failed to start')
            time.sleep(.02)
        run(['cluster', 'init', '--no-listen'])
        run(['-e', "remuda.new('local-gui-proof', {'/bin/sh'})"])
        result = subprocess.run([test_bin, '--ignored', '--exact', 'tests::real_local_daemon_reaches_session_row'], env=env, capture_output=True, text=True, timeout=40)
        print(result.stdout, end='')
        if result.returncode or '1 passed' not in result.stdout:
            raise RuntimeError(f'GUI acceptance failed: {result.stderr}')
    finally:
        try:
            subprocess.run([cli, 'stop', '-f', '--yes'], env=env, capture_output=True, timeout=8)
        except (OSError, subprocess.SubprocessError):
            pass
        try:
            daemon.wait(timeout=3)
        except subprocess.TimeoutExpired:
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
