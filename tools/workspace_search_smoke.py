#!/usr/bin/env python3
"""Exercise workspace search, idle dispatch, previews, and queued edits in a PTY."""

import argparse
from pathlib import Path
import tempfile

from terminal_smoke import Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-workspace-search-") as directory:
        root = Path(directory).resolve()
        (root / ".git").mkdir()
        (root / ".gitignore").write_text("ignored.txt\n")
        (root / "ignored.txt").write_text("needle ignored\n")
        origin = root / "origin.txt"
        target = root / "target.txt"
        origin.write_text("original document\n")
        target.write_text("needle on disk\nrest\n")
        with Terminal([binary, str(origin)], cwd=root) as terminal:
            terminal.start()
            terminal.resize(120, 24)
            terminal.send(b" /needle")
            # Wait for a worker result without sending focus or input events:
            # the debounce deadline must wake the otherwise idle event loop.
            terminal.expect(b"target.txt:1:")
            terminal.expect_screen(b"needle on disk")
            terminal.send(b"\x03")
            terminal.expect_screen(b"original document")
            terminal.send(b" '\r")
            terminal.expect_screen(b"needle on disk")
            terminal.send(b"\x0f")
            terminal.expect_screen(b"original document")
            terminal.send(b" /[")
            terminal.expect_screen(b"Invalid regex")
            terminal.send(b"\x7fneedle\rdiX")
            terminal.expect_screen(b"INS")
            terminal.save_from_insert()
            assert target.read_text() == "Xrest\n"
            assert origin.read_text() == "original document\n"
            # Search the unsaved version of a buffer hidden by ga.
            terminal.send(b"iUNSAVED_")
            terminal.leave_insert()
            terminal.send(b"ga")
            terminal.expect_screen(b"original document")
            terminal.send(b" /UNSAVED_\r")
            terminal.expect_screen(b"UNSAVED_Xrest")
            assert target.read_text() == "Xrest\n"
            terminal.send(b":qa!\r")
            terminal.finish()
        print("PASS: idle search dispatch, preview, last picker, jump back, invalid regex recovery, early acceptance before queued edits, hidden unsaved text, terminal cleanup")


if __name__ == "__main__":
    main()
