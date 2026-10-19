#!/usr/bin/env python3
"""Exercise the native Rust PTY test under a real controlling terminal (Unix).

Usage: python3 scripts/test-exec-pty.py /path/to/muxtop_tui-test-binary
Build it with: cargo test -p muxtop-tui --lib --no-run
"""

import errno
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time


def run_case(binary: str, case: str) -> None:
    pid, master = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        os.environ["MUXTOP_PTY_CASE"] = case
        os.execv(binary, [binary, "exec_tty::tests::t_ac_04_05_06_native_terminal_lifecycle", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
    output = bytearray()
    cursor_replies = 0
    activated = False
    waited = False
    sent = False
    resized_at = None
    deadline = time.monotonic() + 15
    status = None
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                output.extend(chunk)
            while cursor_replies < output.count(b"\x1b[6n"):
                os.write(master, b"\x1b[1;1R")
                cursor_replies += 1
            if b"ACTIVATE EXEC" in output and not activated:
                # One write forces crossterm to prefetch the activation and
                # typeahead together. The handoff discards that typeahead.
                os.write(master, b"eqqx\x1b[20~")
                activated = True
            if b"NATIVE READY" in output and not sent:
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 37, 117, 0, 0))
                os.kill(pid, signal.SIGWINCH)
                resized_at = time.monotonic()
                sent = True
            if resized_at is not None and time.monotonic() - resized_at > 0.3:
                payload = b"\x1d" if case == "detach" else b"raw\x03\x1b[A\xc3\xa9\r"
                os.write(master, payload)
                resized_at = None
        waited, status = os.waitpid(pid, os.WNOHANG)
        if not waited:
            # EOF can precede process teardown by a few milliseconds.
            until = time.monotonic() + 1
            while time.monotonic() < until:
                waited, status = os.waitpid(pid, os.WNOHANG)
                if waited:
                    break
                time.sleep(0.01)
            if not waited:
                raise RuntimeError("native PTY test failed to exit")
        if os.waitstatus_to_exitcode(status) != 0 or f"NATIVE VERIFIED {case}".encode() not in output:
            raise RuntimeError(output.decode("utf-8", errors="replace"))
        for escape in [b"\x1b[?1049l", b"\x1b[?1049h", b"\x1b[?1000l", b"\x1b[?1000h"]:
            if escape not in output:
                raise RuntimeError(f"missing terminal restoration sequence: {escape!r}")
        if b"TUI" not in output or b"RESUMED" not in output:
            raise RuntimeError("TUI was not redrawn")
        print(f"PASS native PTY: {case}")
    finally:
        if not waited:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(master)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    binary = sys.argv[1]
    if binary == "--cargo-json":
        artifacts = (json.loads(line) for line in sys.stdin if line.strip())
        binary = next((item["executable"] for item in artifacts
                       if item.get("executable") and item.get("profile", {}).get("test")), None)
        if binary is None:
            sys.exit("No test binary in cargo JSON output")
    for scenario in ["exit", "detach", "error", "setup-error"]:
        run_case(os.path.abspath(binary), scenario)
