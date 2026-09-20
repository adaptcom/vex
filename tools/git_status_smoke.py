#!/usr/bin/env python3
"""Exercise repository status, fold/navigation state, and idle refresh in a PTY."""
import argparse
from pathlib import Path
import subprocess
import tempfile
import time

from terminal_smoke import Terminal


def git(root, *args):
    return subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-status-") as directory:
        root = Path(directory)
        path = root / "main.txt"
        git(root, "init", "-q")
        git(root, "config", "user.name", "Vex Test")
        git(root, "config", "user.email", "vex@example.invalid")
        git(root, "config", "commit.gpgsign", "false")
        path.write_text("one\ntwo\nthree\n")
        git(root, "add", ".")
        git(root, "commit", "-qm", "base")
        path.write_text("one\nstaged\nthree\n")
        git(root, "add", ".")
        path.write_text("one\ndisk\nthree\n")
        index = (root / ".git/index").read_bytes()
        head = git(root, "rev-parse", "HEAD")
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            mark = terminal.send(b" g")
            terminal.expect(b"Unstaged changes (1)", mark)
            terminal.expect_screen(b"Staged changes (1)")
            mark = terminal.send(b"\t")
            terminal.expect(b"+disk", mark)
            terminal.expect_screen(b"-staged")
            terminal.send(b"n\t")
            terminal.expect_screen("▸ @@".encode())
            terminal.send(b"q g")
            terminal.expect_screen("▸ @@".encode())
            mark = terminal.send(b"\t")
            terminal.expect(b"+disk", mark)
            terminal.send(b"\r")
            terminal.expect_screen(b"2:1")
            terminal.send(b" g wv")
            terminal.expect_screen(b"Git ")
            terminal.send(b" whq")
            terminal.expect_screen(b"disk")
            terminal.send(b" g")
            terminal.expect_screen(b"Unstaged changes (1)")
            # Let focus refreshes finish, then prove an external disk change
            # reaches the status view without sending any new input.
            until = time.monotonic() + 0.25
            while time.monotonic() < until:
                terminal.drain()
            mark = len(terminal.output)
            (root / "new.txt").write_text("untracked\n")
            terminal.expect(b"Untracked files (1)", mark)
            terminal.send(b"q:qa\r")
            terminal.finish()
        assert (root / ".git/index").read_bytes() == index
        assert git(root, "rev-parse", "HEAD") == head
        assert path.read_text() == "one\ndisk\nthree\n"
        print("PASS: status, lazy diffs, folds, file jumps, splits, idle refresh, read-only behavior, cleanup")


if __name__ == "__main__":
    main()
