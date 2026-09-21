#!/usr/bin/env python3
"""Check live Git gutters, idle refresh, and cleanup in the real terminal.

Run after `cargo build --release -p vex_term --locked`. Requires Git and Unix.
Unit tests remain alongside the Rust source.
"""

import argparse
from pathlib import Path
import subprocess
import tempfile
import time

from terminal_smoke import Terminal


def git(root, *args):
    subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-git-smoke-") as directory:
        root = Path(directory)
        path = root / "file [with spaces].txt"
        git(root, "init", "-q")
        git(root, "config", "user.name", "Vex Test")
        git(root, "config", "user.email", "vex@example.invalid")
        git(root, "config", "commit.gpgsign", "false")
        path.write_text("a\nb\nc\nd\ne\n")
        git(root, "add", ".")
        git(root, "commit", "-qm", "initial")
        path.write_text("a\nB\nc\nadded\nd\n")
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            # Await the initial worker result before asking for a full frame:
            # the generic screen helper sends focus events, which refresh Git.
            terminal.expect("▍".encode())
            terminal.expect_screen(" 2 ▍B".encode())
            terminal.expect_screen(" 4 ▍added".encode())
            terminal.expect_screen(" 6 ▔".encode())
            mark = terminal.send(b"iunsaved\r")
            terminal.leave_insert()
            terminal.expect("▍".encode(), mark)
            terminal.expect_screen(" 1 ▍unsaved".encode())
            assert path.read_text() == "a\nB\nc\nadded\nd\n"
            terminal.send(b":w\r")
            terminal.expect_screen(" 1 ▍unsaved".encode())
            assert path.read_text() == "unsaved\na\nB\nc\nadded\nd\n"
            git(root, "add", ".")
            terminal.expect_screen(" 1 ▍unsaved".encode())
            # Let the helper's last focus-triggered HEAD probe finish before
            # changing HEAD, so it cannot stand in for the periodic refresh.
            quiet_until = time.monotonic() + 0.25
            while time.monotonic() < quiet_until:
                terminal.drain()
            mark = len(terminal.output)
            git(root, "commit", "-qm", "editor changes")
            # No keys or focus notifications: the periodic deadline must wake
            # the event loop and erase the first marker in the cell grid.
            terminal.expect(b"\x1b[1;4H\x1b[39m\x1b[49m ", mark)
            terminal.expect_screen(b" 1  unsaved")
            terminal.expect_screen(b" 3  B")
            terminal.expect_screen(b" 5  added")
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: Helix-style markers, unsaved edits, save/stage, idle commit refresh, cleanup")


if __name__ == "__main__":
    main()
