#!/usr/bin/env python3
"""Private daemon + GPUI TestWindow document/session acceptance."""
import os
import pathlib
import struct
import subprocess
import tempfile
import time
import zlib

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
        run(['-e', "remuda.new('document-proof', {'/bin/sh'})"])
        run(['vfs', 'put', '-', 'remuda:///guide.md'], b'# Guide\n\nRead **this** [next](next.md).\n\n![diagram](zz-art.png)\n')
        run(['vfs', 'put', '-', 'remuda:///next.md'], b'# Next\n\nSecond page prose.\n')
        run(['vfs', 'put', '-', 'remuda:///zz-big-a.txt'], b'a' * (1024 * 1024))
        run(['vfs', 'put', '-', 'remuda:///zz-big-b.txt'], b'b' * (1024 * 1024))
        run(['vfs', 'put', '-', 'remuda:///zz-big-a.txt'], b'a' * (1024 * 1024))
        run(['vfs', 'put', '-', 'remuda:///zz-big-b.txt'], b'b' * (1024 * 1024))
        def png_chunk(kind, data):
            return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
        png = (b'\x89PNG\r\n\x1a\n' + png_chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 6, 0, 0, 0))
               + png_chunk(b'IDAT', zlib.compress(b'\x00\xff\x00\x00\xff')) + png_chunk(b'IEND', b''))
        run(['vfs', 'put', '-', 'remuda:///zz-art.png'], png)
        result = subprocess.run([test_bin, '--ignored', '--exact', 'tests::real_terminal_and_two_documents_share_workspace'], env=env, capture_output=True, text=True, timeout=40)
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
            # Signal only our child, never a process group that could have been
            # reused after the daemon exits between poll and killpg.
            daemon.terminate()
            try:
                daemon.wait(timeout=3)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)
