#!/usr/bin/env python3
"""Exercise the real executable and panic cleanup in a Unix pseudo-terminal.

Unit tests remain in the Rust source files. This checks the OS-facing lifecycle,
input parser, and terminal output that an in-memory unit test cannot exercise.
Run after `cargo build --release -p vex_term --locked`.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time


class Terminal:
    def __init__(self, arguments):
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.status = None
        self.restoration, report = os.pipe()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.pid = os.fork()
        if self.pid == 0:
            try:
                os.close(self.restoration)
                os.setsid()
                fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
                for fd in (0, 1, 2):
                    os.dup2(self.slave, fd)
                os.close(self.master)
                if self.slave > 2:
                    os.close(self.slave)
                os.environ["TERM"] = "xterm-256color"
                # Exercise colors even when the surrounding test runner disables
                # them. This only changes the controlled PTY child's environment.
                os.environ.pop("NO_COLOR", None)
                child = os.fork()
                if child == 0:
                    os.execv(arguments[0], arguments)
                # Keep the controlling session alive until after termios is
                # checked. On macOS the slave stops accepting ioctls once its
                # session leader exits, even if the parent retains the fd.
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                signal.signal(signal.SIGHUP, signal.SIG_IGN)
                _, status = os.waitpid(child, 0)
                attributes = termios.tcgetattr(0)
                original = self.original.copy()
                # BSD sets this transient input-queue bit when restoring ICANON;
                # it is kernel state, not a setting changed by the application.
                # See apple-oss-distributions/xnu, bsd/kern/tty.c, TIOCSETA.
                pending = getattr(termios, "PENDIN", 0)
                attributes[3] &= ~pending
                original[3] &= ~pending
                restored = attributes == original
                os.write(report, b"1" if restored else repr((self.original, attributes)).encode())
                code = os.waitstatus_to_exitcode(status)
                os._exit(code if code >= 0 else 128 - code)
            finally:
                os._exit(127)
        os.close(report)
        os.set_blocking(self.master, False)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.status is None:
            # Let the editor stop its workers and language server on test failure.
            self.send(b"\x03:q!\r")
            deadline = time.monotonic() + 2
            while self.poll() is None and time.monotonic() < deadline:
                self.drain()
            if self.status is None:
                os.killpg(self.pid, signal.SIGKILL)
        os.close(self.restoration)
        os.close(self.master)
        os.close(self.slave)
        # Close the PTY before waiting: BSD terminal teardown can wait for output
        # to drain, which cannot happen if this process is blocked in waitpid.
        if self.status is None:
            os.waitpid(self.pid, 0)

    def poll(self):
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)
        return self.status

    def drain(self, timeout=0.025):
        if select.select([self.master], [], [], timeout)[0]:
            while True:
                try:
                    chunk = os.read(self.master, 65536)
                    if not chunk:
                        break
                    self.output.extend(chunk)
                except BlockingIOError:
                    break
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break

    def expect(self, needle, since=0):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            self.drain()
            if needle in self.output[since:]:
                return
            if self.poll() is not None:
                break
        raise AssertionError(f"missing {needle!r}; terminal tail: {bytes(self.output[-2000:])!r}")

    def send(self, data):
        mark = len(self.output)
        os.write(self.master, data)
        return mark

    def expect_screen(self, needle):
        # A worker may complete after the first focus redraw. Ask for complete
        # frames until the expected screen is visible; normal diffs may emit
        # only one changed status digit. No fixed worker-speed assumption.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            mark = self.send(b"\x1b[I")
            self.drain(0.05)
            if needle in self.output[mark:]:
                return
            if self.poll() is not None:
                break
        raise AssertionError(f"missing screen {needle!r}; terminal tail: {bytes(self.output[-2000:])!r}")

    def start(self):
        self.expect(b"\x1b[?2026l")
        assert b"\x1b[?1049h" in self.output, "alternate screen was not entered"
        attrs = termios.tcgetattr(self.slave)
        assert not attrs[3] & (termios.ECHO | termios.ICANON), "terminal is not raw"

    def resize(self, width, height):
        mark = len(self.output)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        os.killpg(self.pid, signal.SIGWINCH)
        self.expect(b"\x1b[2J", mark)

    def finish(self, expected=0, entered=True):
        deadline = time.monotonic() + 5
        while self.poll() is None and time.monotonic() < deadline:
            self.drain()
        self.drain(0)
        assert self.status == expected, (
            f"exit {self.status}, expected {expected}; tail: {bytes(self.output[-2000:])!r}"
        )
        restoration = os.read(self.restoration, 8192)
        assert restoration == b"1", f"termios was not restored: {restoration!r}"
        if entered:
            for sequence in (b"\x1b[?1049l", b"\x1b[?2004l", b"\x1b[?25h", b"\x1b[?7h"):
                assert sequence in self.output, f"missing cleanup {sequence!r}"
        else:
            assert b"\x1b[?1049h" not in self.output


def panic_test_binary():
    result = subprocess.run(
        ["cargo", "test", "-p", "vex_term", "--lib", "--locked", "--no-run", "--message-format=json"],
        check=True, stdout=subprocess.PIPE, text=True,
    )
    for line in result.stdout.splitlines():
        artifact = json.loads(line)
        if artifact.get("executable") and artifact.get("target", {}).get("name") == "vex_term":
            return artifact["executable"]
    raise AssertionError("cargo did not report a terminal unit-test executable")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    arguments = parser.parse_args()
    binary = str(arguments.binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-smoke-") as directory:
        path = Path(directory) / "file with spaces.txt"
        path.write_bytes(b"hello\r\n")
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            mark = terminal.send(b"i")
            terminal.expect(b"INS", mark)
            terminal.send(b"\x1b[200~" + "界e\u0301🦀".encode() + b"\x1b[201~\r")
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            terminal.send(b"uU")
            mark = terminal.send(b":q\r")
            terminal.expect(b"unsaved changes", mark)
            terminal.resize(41, 9)
            # A focus event invalidates the grid. Check the full message, since
            # ordinary diffs may skip letters already present in the old one.
            mark = terminal.send(b"\x1b[200~:q!\r\n\x1b[201~\x1b[I")
            terminal.expect(b"enter insert mode to paste", mark)
            mark = terminal.send(b"\x13")  # Ctrl-s
            terminal.expect(b"wrote", mark)
            assert path.read_bytes() == "界e\u0301🦀\r\nhello\r\n".encode()
            terminal.send(b"\x11")  # Ctrl-q
            terminal.finish()
        print("PASS: Unicode paste, CRLF, undo/redo, dirty quit, resize, save, clean quit")

        saved_group = Path(directory) / "saved group.txt"
        saved_group.write_text("")
        with Terminal([binary, str(saved_group)]) as terminal:
            terminal.start()
            mark = terminal.send(b"ihello\x13")  # Save while still in insert mode.
            terminal.expect(b"wrote", mark)
            assert saved_group.read_text() == "hello"
            mark = terminal.send(b" world\x1b")
            terminal.expect(b"NOR", mark)
            terminal.send(b"u\x11")  # One undo reaches the savepoint; quit must succeed.
            terminal.finish()
            assert saved_group.read_text() == "hello"
        print("PASS: grouped typing and undo to an insert-mode savepoint")

        rust_path = Path(directory) / "highlight.rs"
        rust_path.write_text('fn main() { let message = "界"; }\n')
        with Terminal([binary, str(rust_path)]) as terminal:
            terminal.start()
            # Completion must wake an idle terminal without another input event.
            terminal.expect(b"\x1b[38;5;13m")
            mark = terminal.send(b"i")
            terminal.expect(b"INS", mark)
            mark = terminal.send(b"//\x1b")
            terminal.expect(b"NOR", mark)
            terminal.expect(b"\x1b[38;5;8m", mark)  # Entire line becomes a comment.
            mark = terminal.send(b"u")
            terminal.expect(b"\x1b[38;5;13m", mark)  # Undo produces fresh keywords.
            terminal.send(b"\x11")
            terminal.finish()
            assert rust_path.read_text() == 'fn main() { let message = "界"; }\n'
        print("PASS: idle background syntax completion, comment edit, undo, and clean quit")

        search_path = Path(directory) / "search.txt"
        search_path.write_text("start cat one\nmid cat two\nend cat three\n")
        with Terminal([binary, str(search_path)]) as terminal:
            terminal.start()
            # Force a full redraw when inspecting status text: a normal diff
            # can emit only the changed column digit, omitting the row and colon.
            mark = terminal.send(b"/cat\x1b[I")
            terminal.expect_screen(b"1:9")
            mark = terminal.send(b"\rn\x1b[I")
            terminal.expect_screen(b"2:7")
            mark = terminal.send(b"/missing\x1b[I")
            terminal.expect_screen(b"no matches")
            mark = terminal.send(b"\x1b")
            terminal.expect(b"\x1b[?2026l", mark)
            mark = terminal.send(b"n\x1b[I")
            terminal.expect_screen(b"3:7")
            mark = terminal.send(b"?cat\rn\x1b[I")
            terminal.expect_screen(b"2:5")
            mark = terminal.send(b"d\x13")
            terminal.expect(b"wrote", mark)
            assert search_path.read_text() == "start cat one\nmid  two\nend cat three\n"
            terminal.send(b"\x11")
            terminal.finish()
        print("PASS: search preview, accept, cancel, forward/backward repeats, and edit match")

        ordered_path = Path(directory) / "ordered search.txt"
        ordered_path.write_text("x cat cat")
        with Terminal([binary, str(ordered_path)]) as terminal:
            terminal.start()
            # Enter and edit keys arrive before either search result. Both the
            # preview and the n repeat must resolve before d and save execute.
            mark = terminal.send(b"/cat\rnd\x13")
            terminal.expect(b"wrote", mark)
            assert ordered_path.read_text() == "x cat "
            terminal.send(b"\x11")
            terminal.finish()
        print("PASS: early search acceptance and queued repeat/edit/save preserve key order")

        burst_path = Path(directory) / "input burst.txt"
        burst_path.write_text("")
        with Terminal([binary, str(burst_path)]) as terminal:
            terminal.start()
            # Stay below 1 KiB to avoid overflowing the PTY's own input queue.
            mark = terminal.send(b"i" + b"abcdef" * 150)
            terminal.expect(b"INS", mark)
            # This spans many bounded input batches; no key may be consumed
            # past the batch limit and then lost before the next iteration.
            mark = terminal.send(b"\x13")
            terminal.expect(b"wrote", mark)
            assert burst_path.read_text() == "abcdef" * 150
            terminal.send(b"\x11")
            terminal.finish()
        print("PASS: bounded event batches preserve all 900 queued text keys")

        with Terminal([binary]) as terminal:
            terminal.start()
            mark = terminal.send(b"i")
            terminal.expect(b"INS", mark)
            terminal.send(b"discard this")
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            terminal.send(b":q!\r")
            terminal.finish()
        print("PASS: forced quit restores terminal")

        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            os.killpg(terminal.pid, signal.SIGTERM)
            terminal.finish(expected=1)
        print("PASS: SIGTERM restores terminal")

        path.write_bytes(b"\xff")
        with Terminal([binary, str(path)]) as terminal:
            terminal.finish(expected=1, entered=False)
        print("PASS: invalid UTF-8 fails without altering terminal")

        long_path = Path(directory) / "long-line.txt"
        long_text = b"x" * (1 << 20) + b"END"
        long_path.write_bytes(long_text)
        with Terminal([binary, str(long_path)]) as terminal:
            terminal.start()
            mark = terminal.send(b"ge")
            terminal.expect(b"END", mark)
            mark = terminal.send(b"i")
            terminal.expect(b"INS", mark)
            terminal.send(b"!")
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            terminal.send(b":wq\r")
            terminal.finish()
            assert long_path.read_bytes() == long_text + b"!"
            assert len(terminal.output) < 100_000, "hidden text was written to the terminal"
        print("PASS: seek, edit, and save at the end of a 1 MiB line")

    with Terminal([
        panic_test_binary(), "--exact", "terminal::tests::panic_restores_terminal", "--nocapture",
    ]) as terminal:
        terminal.finish()
        assert b"intentional terminal-cleanup test" in terminal.output
        assert b"test result: ok" in terminal.output
        assert terminal.output.index(b"\x1b[?1049l") < terminal.output.index(b"intentional terminal-cleanup test")
    print("PASS: panic restores terminal before printing diagnostic")


if __name__ == "__main__":
    main()
