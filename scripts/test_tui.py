#!/usr/bin/env python3
"""Real-PTY regression tests (Linux, Python standard library only).

Build first: cargo build -p turnip-app
Run: python3 scripts/test_tui.py [path/to/turnip]

Unit tests use a VT100 emulator for screen/cursor correctness. These tests
exercise the actual event loops, startup pickers, resize, paste, and streaming,
and assert that idle terminals produce *no* output. No real API is contacted.
"""

import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest


BINARY = Path(sys.argv.pop(1) if len(sys.argv) > 1 else "target/debug/turnip").resolve()


class App:
    def __init__(self, url, width=80, height=24, provider="openai", model="mock-1"):
        self.root = tempfile.TemporaryDirectory(prefix="turnip-tui-")
        root = Path(self.root.name)
        config = root / "config" / "turnip"
        config.mkdir(parents=True)
        (config / "config.json").write_text(json.dumps({
            "providers": {"openai": {"base_url": url}},
        }))
        env = {key: value for key, value in os.environ.items() if not key.startswith("TURNIP_")}
        env.update(
            TERM="xterm-256color",
            HOME=str(root),
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_STATE_HOME=str(root / "state"),
            TURNIP_OPENAI_API_KEY="mock-key",
        )
        args = [str(BINARY), "--provider", provider, "--bare", "--no-session"]
        if model is not None:
            args += ["--model", model]
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
                os.chdir(root)
                os.execve(BINARY, args, env)
            finally:
                os._exit(127)
        self.status = None

    def read_for(self, duration):
        data = bytearray()
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([self.fd], [], [], max(0, deadline - time.monotonic()))[0]:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                data.extend(chunk)
        return bytes(data)

    def until(self, marker, timeout=5):
        data = bytearray()
        deadline = time.monotonic() + timeout
        while marker not in data and time.monotonic() < deadline:
            data.extend(self.read_for(0.05))
        assert marker in data, f"missing {marker!r} in {bytes(data)!r}"
        return bytes(data)

    def idle(self):
        # Drain event-triggered output, then observe several old redraw periods.
        self.read_for(0.3)
        data = self.read_for(0.3)
        assert not data, f"idle app kept writing ({len(data)} bytes): {data[:300]!r}"
        assert os.waitpid(self.pid, os.WNOHANG) == (0, 0), "app exited unexpectedly"

    def send(self, data):
        os.write(self.fd, data)

    def resize(self, width, height):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def exit(self, code=0):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
                assert os.waitstatus_to_exitcode(status) == code, status
                flags = termios.tcgetattr(self.fd)[3]
                assert flags & termios.ICANON and flags & termios.ECHO, "raw mode not restored"
                return
            self.read_for(0.05)
        raise AssertionError("app did not exit")

    def __enter__(self):
        return self

    def __exit__(self, *args):
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except ProcessLookupError:
                pass
        os.close(self.fd)
        self.root.cleanup()


class TuiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Keep the mock in a separate process: forking a PTY from a
        # multithreaded Python process is unsafe on some platforms.
        cls.server = subprocess.Popen(
            [sys.executable, "-B", "-u", "-c", (
                "from http.server import HTTPServer; "
                "from mock_openai_server import Handler; "
                "server = HTTPServer(('127.0.0.1', 0), Handler); "
                "print(server.server_port); server.serve_forever()"
            )],
            cwd=Path(__file__).resolve().parent,
            stdout=subprocess.PIPE,
            text=True,
        )
        port = int(cls.server.stdout.readline())
        cls.url = f"http://127.0.0.1:{port}/v1"

    @classmethod
    def tearDownClass(cls):
        cls.server.terminate()
        cls.server.wait(timeout=3)
        cls.server.stdout.close()

    def test_idle_repl_at_multiple_sizes(self):
        for width, height in [(80, 24), (40, 12), (12, 4), (1, 1)]:
            with self.subTest(size=(width, height)), App(self.url, width, height) as app:
                app.until(b"\x1b[?2004h")
                app.idle()
                app.send(b"\x03")
                app.exit()

    def test_startup_provider_picker_is_idle_and_escape_cancels(self):
        # This provider deliberately has no endpoint, so startup opens a picker.
        with App(self.url, 40, 12, provider="openai-compatible") as app:
            app.until(b"Choose a provider")
            app.idle()
            app.send(b"\x1b[200~openai\x1b[201~")
            app.idle()
            app.resize(20, 5)
            app.idle()
            app.send(b"\x1b")
            app.exit(code=1)

    def test_startup_model_picker_is_idle_and_selects(self):
        with App(self.url, 40, 12, model=None) as app:
            app.until(b"Choose a model")
            app.idle()
            app.send(b"\r")
            app.until("› ".encode())
            app.idle()
            app.send(b"/quit\r")
            app.exit()

    def test_edit_paste_resize_clear_and_modal_cancel(self):
        with App(self.url, 40, 12) as app:
            app.until("› ".encode())
            app.idle()
            app.send(b"\x1b[200~" + ("  wide 界 text\n" * 20).encode() + b"\x1b[201~")
            app.idle()
            app.resize(16, 5)
            app.idle()
            app.resize(80, 24)
            app.idle()
            app.send(b"\x03/clear\r")
            app.idle()
            app.send(b"/model\r")
            app.until(b"Choose a model")
            app.idle()
            app.send(b"\x1b")
            app.idle()
            app.send(b"/help\r")
            app.until(b"show this help")
            app.idle()
            app.send(b"/quit\r")
            app.exit()

    def test_streaming_tool_turn_returns_to_an_idle_prompt(self):
        with App(self.url, 40, 12) as app:
            app.until("› ".encode())
            app.send(b"hello\r")
            output = app.until(b"Done.")
            self.assertIn(b"hello-from-tool", output)
            app.idle()
            app.send(b"/quit\r")
            app.exit()


if __name__ == "__main__":
    if not BINARY.is_file():
        raise SystemExit(f"build the app first: {BINARY} not found")
    unittest.main(verbosity=2)
