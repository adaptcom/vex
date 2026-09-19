#!/usr/bin/env python3
"""Exercise key groups and the file picker through Vex's real terminal event loop.

Run after cargo build --release -p vex_term --locked. Unit tests remain in Rust.
"""

import argparse
from pathlib import Path
import tempfile

from terminal_smoke import Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-picker-") as directory:
        root = Path(directory).resolve()
        (root / ".git").mkdir()
        (root / ".gitignore").write_text("ignored/\n*.log\n")
        (root / "ignored").mkdir()
        (root / "ignored/secret.txt").write_text("not listed")
        (root / "src").mkdir()
        origin = root / "origin.txt"
        target = root / "src/界 target.txt"
        origin.write_text("original document\n")
        target.write_text("unique preview contents\n")
        (root / "src/preview.rs").write_text('fn main() { let text = "hello"; }\n')
        with Terminal([binary, str(origin)]) as terminal:
            terminal.start()
            terminal.resize(120, 20)
            terminal.send(b"v ")
            terminal.expect_screen("Space · Esc cancel".encode())
            terminal.send(b"\x03")  # Cancel and return to normal mode.
            terminal.send(b" f")
            terminal.expect_screen("Files ·".encode())
            terminal.expect_screen("┌─".encode())
            terminal.expect_screen(b"original document")
            mark = terminal.send(b"preview.rs")
            terminal.expect_screen("Preview · src/preview.rs".encode())
            terminal.expect_screen(b'fn main() { let text = "hello"; }')
            terminal.expect(b"\x1b[38;5;13m\x1b[49mfn", mark)
            terminal.expect(b'\x1b[38;5;10m\x1b[49m"hello"', mark)
            terminal.send(b"\x03")
            terminal.expect_screen(b"original document")
            terminal.send(b" f")
            terminal.send("界".encode())
            terminal.expect_screen(b"unique preview contents")
            terminal.resize(44, 9)
            terminal.expect_screen("界 target.txt".encode())
            terminal.send(b"\x03")
            terminal.expect_screen(b"original document")
            # Paste and early Enter in one burst, followed by edit/save. The
            # query's completion must resolve before editing keys are dispatched.
            terminal.send(b" f\x1b[200~" + "界 target".encode() + b"\x1b[201~\riX\x13")
            terminal.expect_screen(b"wrote")
            assert target.read_text() == "Xunique preview contents\n"
            assert origin.read_text() == "original document\n"
            terminal.send(b"\x03\x0f")
            terminal.expect_screen(b"original document")
            terminal.send(b"iDIRTY\x03")
            terminal.send(b" ftarget\r")
            terminal.expect_screen(b"Save this buffer")
            terminal.send(b"\x03:q!\r")
            terminal.finish()
        print("PASS: group hints, floating borders, visible original buffer, syntax preview, Unicode fuzzy query, resize, cancellation, early Enter/edit/save, jump back, dirty-buffer protection, terminal cleanup")


if __name__ == "__main__":
    main()
