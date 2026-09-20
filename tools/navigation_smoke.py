#!/usr/bin/env python3
"""Exercise asynchronous navigation followed by queued edits through the real PTY."""

import argparse
from pathlib import Path
import tempfile

from terminal_smoke import Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-navigation-") as directory:
        path = Path(directory) / "text.txt"
        path.write_text("alpha beta\n")
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            terminal.send(b"iXY")
            terminal.leave_insert()
            terminal.send(b"glg.diZ")  # Jump must finish before delete/insert.
            terminal.expect_screen(b"INS")
            terminal.save_from_insert()
            assert path.read_text() == "XYZlpha beta\n"
            # The g. origin was at the line end. Ctrl-o returns there, while
            # the delete + single-character insert left its scalar position valid.
            terminal.send(b"\x0fi!")
            terminal.save_from_insert()
            assert path.read_text() == "XYZlpha bet!a\n"
            terminal.send(b":q\r")
            terminal.finish()
        path.write_text("alpha\nbeta\ngamma\n")
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            terminal.send(b"2gg\x133gg jtext.txt:2\ri!")
            terminal.expect_screen(b"INS")
            terminal.save_from_insert()
            assert path.read_text() == "alpha\n!beta\ngamma\n"
            terminal.send(b"\x0fi?")  # The return checkpoint follows the earlier ! insertion.
            terminal.save_from_insert()
            assert path.read_text() == "alpha\n!beta\n?gamma\n"
            # A stored selection follows edits before it, including when the
            # jump picker is reopened after those changes.
            terminal.send(b"ggiprefix\n")
            terminal.leave_insert()
            terminal.send(b" jtext.txt:3 b\ri>")
            terminal.save_from_insert()
            assert path.read_text() == "prefix\nalpha\n!>beta\n?gamma\n"
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: g. and jump picker destinations, queued edits, remapped jump origins, save, terminal cleanup")


if __name__ == "__main__":
    main()
