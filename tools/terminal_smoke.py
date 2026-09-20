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
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time


class Terminal:
    def __init__(self, arguments, env=None):
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
                if env:
                    os.environ.update(env)
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
            for _ in range(3):
                self.send(b"\x1b")
                self.drain(0.05)
            self.send(b":qa!\r")
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

    def leave_insert(self):
        # Keep Escape separate from the next printable key: adjacent bytes can
        # be decoded as an Alt chord by the terminal input parser.
        self.expect_screen(b"INS")
        mark = self.send(b"\x1b")
        self.expect(b"NOR", mark)

    def save_from_insert(self):
        self.leave_insert()
        mark = self.send(b":w\r")
        self.expect(b"wrote", mark)

    def expect_screen(self, needle):
        # A worker may complete after the first focus redraw. Ask for complete
        # frames until the expected screen is visible; normal diffs may emit
        # only one changed status digit. No fixed worker-speed assumption.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            mark = self.send(b"\x1b[I")
            self.drain(0.05)
            # A label can cross cursor, syntax, or fuzzy-match style changes.
            # Raw escape-sequence assertions still use expect().
            visible = re.sub(rb"\x1b\[[0-9;:]*m", b"", self.output[mark:])
            if needle in visible:
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
            mark = terminal.send(b":w\r")
            terminal.expect(b"wrote", mark)
            assert path.read_bytes() == "界e\u0301🦀\r\nhello\r\n".encode()
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: Unicode paste, CRLF, undo/redo, dirty quit, resize, save, clean quit")

        opened_lines = Path(directory) / "opened lines.txt"
        opened_lines.write_bytes(b"\tfirst\r\nlast")
        with Terminal([binary, str(opened_lines)]) as terminal:
            terminal.start()
            # Exercise both common Backspace bytes through Crossterm, too.
            terminal.send(b"obeloX\x7fwY\x08")
            terminal.expect_screen(b"below")
            terminal.leave_insert()
            terminal.send(b"Oabove")
            terminal.expect_screen(b"above")
            terminal.leave_insert()
            terminal.send(b"u:wq\r")
            terminal.finish()
            assert opened_lines.read_bytes() == b"\tfirst\r\n\tbelow\r\nlast"
        print("PASS: o/O, indentation, CRLF, grouped undo, both Backspace encodings")

        indented_lines = Path(directory) / "indented lines.txt"
        indented_lines.write_bytes(b"\t  first\r\nlast")
        with Terminal([binary, str(indented_lines)]) as terminal:
            terminal.start()
            terminal.send(b"gla\rsecond")
            terminal.expect_screen(b"second")
            terminal.leave_insert()
            terminal.send(b"u")
            terminal.expect_screen(b"first")
            # Redo must restore the newline, mixed indentation, and typed text.
            terminal.send(b"U:wq\r")
            terminal.finish()
            assert indented_lines.read_bytes() == b"\t  first\r\n\t  second\r\nlast"
        print("PASS: Enter copies mixed indentation, preserves CRLF, and groups undo/redo")

        saved_group = Path(directory) / "saved group.txt"
        saved_group.write_text("")
        with Terminal([binary, str(saved_group)]) as terminal:
            terminal.start()
            terminal.send(b"ihello\x13")  # Explicit undo checkpoint, without I/O.
            terminal.expect_screen(b"hello")
            assert saved_group.read_text() == ""
            terminal.send(b" world")
            terminal.leave_insert()
            terminal.send(b"u:wq\r")
            terminal.finish()
            assert saved_group.read_text() == "hello"
        print("PASS: Ctrl-s splits typing undo groups without saving")

        pages = Path(directory) / "pages.txt"
        page_source = "".join(f"row {line:03}\n" for line in range(100))
        pages.write_text(page_source)
        with Terminal([binary, str(pages)]) as terminal:
            terminal.start()
            terminal.resize(80, 12)  # Ten text rows: five lines per half page.
            terminal.send(b"20j2l\x04")
            terminal.expect_screen(b"26:3")
            terminal.send(b"\x15")
            terminal.expect_screen(b"21:3")
            terminal.send(b"2\x04")
            terminal.expect_screen(b"31:3")
            terminal.resize(80, 22)
            terminal.send(b"\x15")
            terminal.expect_screen(b"21:3")
            terminal.send(b":q\r")
            terminal.finish()
        assert pages.read_text() == page_source
        print("PASS: Ctrl-u/Ctrl-d half-page movement, counts, resize, and clean quit")

        copied_path = Path(directory) / "copied selections.txt"
        copied_path.write_text("a1\nb2\nc3\n")
        with Terminal([binary, str(copied_path)]) as terminal:
            terminal.start()
            # The delete/save must wait for the worker's new selections.
            terminal.send(b"2Cd:wq\r")
            terminal.finish()
        assert copied_path.read_text() == "1\n2\n3\n"
        print("PASS: counted C creates selections before queued delete/save/quit")

        split_path = Path(directory) / "split.txt"
        split_path.write_text("alpha\nsecond\n")
        other_path = Path(directory) / "other.txt"
        other_path.write_text("beta\n")
        with Terminal([binary, str(split_path)]) as terminal:
            terminal.start()
            terminal.send(b"\x17v")  # Ctrl-w v
            terminal.expect_screen("│".encode())
            terminal.send(b"iX")
            terminal.leave_insert()
            terminal.send(b" whu")  # Focus left, shared undo.
            terminal.expect_screen(b"alpha")
            terminal.send(b"U:w\r")
            terminal.expect_screen(b"wrote")
            assert split_path.read_text() == "Xalpha\nsecond\n"
            terminal.send(b" wl\x17\x13")  # Ctrl-w Ctrl-s must split, not save.
            terminal.expect_screen(b"Xalpha")
            terminal.send(b"\x17\x11")  # Ctrl-w Ctrl-q closes only this window.
            terminal.expect_screen("│".encode())
            assert terminal.poll() is None
            terminal.send(b" wo:vsplit " + os.fsencode(other_path) + b"\r")
            terminal.expect_screen(b"beta")
            terminal.expect_screen(b"Xalpha")
            terminal.send(b"iY")
            terminal.leave_insert()
            terminal.send(b":q\r")
            terminal.expect_screen(b"Xalpha")
            terminal.send(b":q\r")  # Final quit also protects hidden buffers.
            terminal.expect_screen(b"unsaved changes")
            terminal.send(b":vsplit " + os.fsencode(other_path) + b"\r:w\r")
            terminal.expect_screen(b"wrote")
            terminal.send(b" wH")  # Swap the active file left.
            terminal.resize(10, 3)
            # Confirm the small resize was handled, rather than mistaking an
            # earlier queued focus redraw for its clear-screen sequence.
            terminal.send(b":help\r")
            terminal.expect_screen(b"\x1b[3;1Hi/a insert")
            terminal.resize(80, 24)
            terminal.expect_screen("│".encode())
            terminal.send(b":q\r")
            terminal.expect_screen(b"Xalpha")
            assert terminal.poll() is None
            terminal.send(b":q\r")
            terminal.finish()
            assert other_path.read_text() == "Ybeta\n"
        print("PASS: window prefixes, shared undo/save, distinct buffers, close protection, swap, resize")

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
            terminal.send(b":q\r")
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
            terminal.expect_screen(b"3:7")
            mark = terminal.send(b"d:w\r")
            terminal.expect(b"wrote", mark)
            assert search_path.read_text() == "start cat one\nmid cat two\nend  three\n"
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: search preview, accept, cancel, forward/backward repeats, and edit match")

        ordered_path = Path(directory) / "ordered search.txt"
        ordered_path.write_text("x cat cat")
        with Terminal([binary, str(ordered_path)]) as terminal:
            terminal.start()
            # Enter and edit keys arrive before either search result. Both the
            # preview and the n repeat must resolve before d and save execute.
            mark = terminal.send(b"/cat\rnd:w\r")
            terminal.expect(b"wrote", mark)
            assert ordered_path.read_text() == "x cat "
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: early search acceptance and queued repeat/edit/save preserve key order")

        selections_path = Path(directory) / "regex selections.txt"
        selections_path.write_text("one two three")
        with Terminal([binary, str(selections_path)]) as terminal:
            terminal.start()
            terminal.send(b"%s[a-z]+\rKo\rd:w\r")
            terminal.expect_screen(b"wrote")
            assert selections_path.read_text() == "  three"
            terminal.send(b"u%S +\rd:wq\r")
            terminal.finish()
            assert selections_path.read_text() == "  "
        print("PASS: regex select, filter, split, and ordered edits through the worker")

        textobjects_path = Path(directory) / "textobjects.txt"
        textobjects_path.write_text("alpha beta\n\nsecond para\n\nlast\n")
        with Terminal([binary, str(textobjects_path)]) as terminal:
            terminal.start()
            terminal.send(b"miwd2mipd:w\r:q\r")
            terminal.finish()
        assert textobjects_path.read_text() == "\nlast\n"
        print("PASS: word/paragraph textobjects complete before queued delete/save/quit")

        surrounds_path = Path(directory) / "surrounds.txt"
        surrounds_path.write_text("alpha beta\n")
        with Terminal([binary, str(surrounds_path)]) as terminal:
            terminal.start()
            terminal.send(b"miwms)uU:w\r:q\r")
            terminal.finish()
        assert surrounds_path.read_text() == "(alpha) beta\n"
        print("PASS: textobject selection then surround, undo, redo, and save")

        delimiters_path = Path(directory) / "delimiters.txt"
        delimiters_path.write_text("(alpha) [beta]\n")
        with Terminal([binary, str(delimiters_path)]) as terminal:
            terminal.start()
            terminal.send(b"mmmi)d:w\r:q\r")
            terminal.finish()
        assert delimiters_path.read_text() == "() [beta]\n"
        print("PASS: bracket matching and delimiter objects precede queued edits")

        edit_surrounds_path = Path(directory) / "edit-surrounds.txt"
        edit_surrounds_path.write_text("(alpha) [beta]\n")
        with Terminal([binary, str(edit_surrounds_path)]) as terminal:
            terminal.start()
            terminal.send(b"lvmr)}uUmd}uU:w\r:q\r")
            terminal.finish()
        assert edit_surrounds_path.read_text() == "alpha [beta]\n"
        print("PASS: staged surround replacement, deletion, undo, redo, and save")

        burst_path = Path(directory) / "input burst.txt"
        burst_path.write_text("")
        with Terminal([binary, str(burst_path)]) as terminal:
            terminal.start()
            # Stay below 1 KiB to avoid overflowing the PTY's own input queue.
            mark = terminal.send(b"i" + b"abcdef" * 150)
            terminal.expect(b"INS", mark)
            # This spans many bounded input batches; no key may be consumed
            # past the batch limit and then lost before the next iteration.
            terminal.save_from_insert()
            assert burst_path.read_text() == "abcdef" * 150
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: bounded event batches preserve all 900 queued text keys")

        repeat_path = Path(directory) / "repeat.txt"
        repeat_path.write_text("a\n")
        with Terminal([binary, str(repeat_path)]) as terminal:
            terminal.start()
            mark = terminal.send(b"aX")
            terminal.expect(b"INS", mark)
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            # Replay spans multiple event-loop batches. The queued colon/save
            # keys must wait for all 100 sessions to finish in normal mode.
            terminal.send(b"100.:wq\r")
            terminal.finish()
            assert repeat_path.read_text() == "a" + "X" * 101 + "\n"
        print("PASS: counted insert replay and queued save preserve input order")

        # Isolate clipboard helpers from the desktop clipboard. A slow helper
        # makes copy/paste overlap queued edits and save commands in the PTY.
        helper_dir = Path(directory) / "clipboard-bin"
        helper_dir.mkdir()
        clipboard_file = Path(directory) / "private-clipboard"
        clipboard_file.write_text("")
        for name, body in [
            ("pbcopy", 'cat > "$VEX_SMOKE_CLIPBOARD"'),
            ("pbpaste", 'cat "$VEX_SMOKE_CLIPBOARD"'),
            ("termux-clipboard-set", 'cat > "$VEX_SMOKE_CLIPBOARD"'),
            ("termux-clipboard-get", 'cat "$VEX_SMOKE_CLIPBOARD"'),
        ]:
            helper = helper_dir / name
            helper.write_text(f"#!/bin/sh\nsleep 0.05\n{body}\n")
            helper.chmod(0o700)
        clipboard_env = {
            "PATH": str(helper_dir) + os.pathsep + os.environ.get("PATH", "/usr/bin:/bin"),
            "TMUX": "", "VEX_SMOKE_CLIPBOARD": str(clipboard_file),
        }
        clipboard_path = Path(directory) / "clipboard.txt"
        original = "e\u0301界\r\n".encode()
        clipboard_path.write_bytes(original)
        with Terminal([binary, str(clipboard_path)], env=clipboard_env) as terminal:
            terminal.start()
            terminal.send(b"% yd puU:wq\r")
            terminal.finish()
            assert clipboard_path.read_bytes() == original
            assert clipboard_file.read_bytes() == original
        clipboard_path.write_text("abc")
        clipboard_file.write_text("X")
        with Terminal([binary, str(clipboard_path)], env=clipboard_env) as terminal:
            terminal.start()
            terminal.send(b"2 P:wq\r")
            terminal.finish()
            assert clipboard_path.read_text() == "XXabc"
        print("PASS: background clipboard copy/paste, CRLF, counts, undo, and queued save")

        clipboard_path.write_text("abc")
        clipboard_file.write_text("old")
        with Terminal([binary, str(clipboard_path)], env=clipboard_env) as terminal:
            terminal.start()
            mark = terminal.send(b'"+cX')
            terminal.expect(b"INS", mark)
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            terminal.send(b"l.uU:wq\r")
            terminal.finish()
            assert clipboard_path.read_text() == "XXc"
            assert clipboard_file.read_text() == "b"
        clipboard_path.write_text("ab")
        clipboard_file.write_text("X")
        with Terminal([binary, str(clipboard_path)], env=clipboard_env) as terminal:
            terminal.start()
            mark = terminal.send(b"a\x12+")
            terminal.expect(b"X", mark)
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            clipboard_file.write_text("Y")
            terminal.send(b"2.:wq\r")
            terminal.finish()
            assert clipboard_path.read_text() == "aXYYb"
        clipboard_path.write_text("cat dog cat dog")
        clipboard_file.write_text("dog")
        with Terminal([binary, str(clipboard_path)], env=clipboard_env) as terminal:
            terminal.start()
            terminal.send(b'"+2nd:wq\r')
            terminal.finish()
            assert clipboard_path.read_text() == "cat dog cat "
        print("PASS: clipboard registers, delayed changes, live insert replay, search, and queued edits")

        register_path = Path(directory) / "registers.txt"
        register_path.write_text("cat")
        with Terminal([binary, str(register_path)]) as terminal:
            terminal.start()
            mark = terminal.send(b'%"ay"_di\x12a')
            terminal.expect(b"INS", mark)
            mark = terminal.send(b"\x1b")
            terminal.expect(b"NOR", mark)
            terminal.send(b'"aP:wq\r')
            terminal.finish()
            assert register_path.read_text() == "cacatt"
        print("PASS: named register, discard deletion, insert Ctrl-r, and queued save")

        register_path.write_text("cat dog cat dog")
        with Terminal([binary, str(register_path)]) as terminal:
            terminal.start()
            mark = terminal.send(b'"a/dog\rn"bd:w\r')
            terminal.expect(b"wrote", mark)
            terminal.send(b":\x12:\r:q\r")
            terminal.finish()
            assert register_path.read_text() == "cat dog cat "
        print("PASS: named search register, queued repeat/cut, and last-command insertion")

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
