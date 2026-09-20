#!/usr/bin/env python3
"""Exercise stage, unstage, commit drafts, hooks, and worker completion in a PTY.

All Git writes are confined to the temporary fixture repository.
"""
import argparse
from pathlib import Path
import tempfile
import re
import time

from git_status_smoke import git
from terminal_smoke import Terminal


def settle(terminal):
    until = time.monotonic() + 0.3
    while time.monotonic() < until:
        terminal.drain()


def screen(terminal, needle):
    # Resize forces a complete redraw without restarting status queries. Focus
    # redraws can repeatedly cancel an in-flight refresh on slower machines.
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        width = 81 if getattr(terminal, "git_width", 80) == 80 else 80
        terminal.git_width = width
        mark = len(terminal.output)
        terminal.resize(width, 24)
        terminal.drain(0.05)
        visible = re.sub(rb"\x1b\[[0-9;:]*m", b"", terminal.output[mark:])
        if needle in visible:
            return
    raise AssertionError(f"missing {needle!r}; terminal tail: {bytes(terminal.output[-3500:])!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-git-write-") as directory:
        root = Path(directory)
        path = root / "file.txt"
        git(root, "init", "-q")
        git(root, "config", "user.name", "Vex Test")
        git(root, "config", "user.email", "vex@example.invalid")
        git(root, "config", "commit.gpgsign", "false")
        git(root, "config", "core.hooksPath", str(root / ".git/hooks"))
        path.write_text("base\n")
        git(root, "add", ".")
        git(root, "commit", "-qm", "base")
        path.write_text("saved disk\n")
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            mark = terminal.send(b" g")
            terminal.expect(b"Unstaged changes (1)", mark)
            terminal.send(b"s")
            screen(terminal, b"Stage complete")
            screen(terminal, b"Staged changes (1)")
            assert git(root, "show", ":file.txt") == b"saved disk\n"
            terminal.send(b"u")
            screen(terminal, b"Unstage complete")
            screen(terminal, b"Unstaged changes (1)")
            assert git(root, "diff", "--cached", "--name-only") == b""
            terminal.send(b"s")
            screen(terminal, b"Stage complete")
            screen(terminal, b"Staged changes (1)")

            terminal.send(b"ccFirst subject\r\rBody of message")
            screen(terminal, b"Git commit")
            terminal.send(b"\x03\x0b")  # Ctrl-c Ctrl-k retains the draft.
            screen(terminal, b"Commit draft retained")
            terminal.send(b"cc")
            screen(terminal, b"First subject")

            hook = root / ".git/hooks/pre-commit"
            hook.write_text("#!/bin/sh\nprintf 'fixture hook declined\\n' >&2\nexit 1\n")
            hook.chmod(0o755)
            head = git(root, "rev-parse", "HEAD")
            terminal.send(b"\x03\x03")
            screen(terminal, b"Commit failed")
            screen(terminal, b"fixture hook declined")
            screen(terminal, b"First subject")
            assert git(root, "rev-parse", "HEAD") == head

            # A running hook outlives the composer. Stage/quit attempts report
            # busy while the UI remains responsive; release it from this test.
            hook.write_text("#!/bin/sh\nwhile [ ! -f release-hook ]; do sleep 0.05; done\n")
            terminal.send(b"\x03\x03\x03\x0b")
            screen(terminal, b"Commit draft retained")
            terminal.send(b":qa!\r")
            screen(terminal, b"Git operation still running")
            settle(terminal)
            (root / "release-hook").touch()
            screen(terminal, b"Commit complete")
            assert git(root, "log", "-1", "--format=%B").strip() == b"First subject\n\nBody of message"
            assert git(root, "show", "HEAD:file.txt") == b"saved disk\n"
            terminal.send(b"cc")
            screen(terminal, b"Git commit")
            terminal.send(b"\x03\x03")
            screen(terminal, b"commit message is empty")
            terminal.send(b"\x03\x0b:qa\r")
            terminal.finish()
        assert path.read_text() == "saved disk\n"
        print("PASS: stage, unstage, drafts, hook errors, ordered writes, commit, busy quit, terminal cleanup")


if __name__ == "__main__":
    main()
