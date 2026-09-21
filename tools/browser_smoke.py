#!/usr/bin/env python3
"""Exercise directory browsing through Vex's real terminal event loop."""

import argparse
from pathlib import Path
import tempfile

from terminal_smoke import Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    binary = str(parser.parse_args().binary.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-browser-") as directory:
        root = Path(directory).resolve()
        (root / ".git").mkdir()
        (root / "src").mkdir()
        (root / "empty directory").mkdir()
        origin = root / "origin.rs"
        target = root / "src/界 target.rs"
        origin.write_text("fn original() {}\n")
        target.write_text("fn destination() {}\n")
        with Terminal([binary, str(origin)]) as terminal:
            terminal.start()
            terminal.resize(120, 20)
            terminal.send(b" e")
            terminal.expect_screen_idle("Browse")
            terminal.expect_screen_idle("Preview · origin.rs")
            terminal.send(b"src")
            terminal.expect_screen_idle("Preview · src/")
            terminal.expect_screen_idle("界 target.rs")
            terminal.send(b"\r")
            terminal.expect_screen_idle("Preview · 界 target.rs")
            terminal.expect_screen_idle("fn destination() {}")
            terminal.resize(44, 9)
            terminal.expect_screen("界 target.rs".encode())
            terminal.send(b"\x7f")  # Empty query: restore the parent and its filter.
            terminal.expect_screen(b"src/")
            terminal.resize(120, 20)
            terminal.expect_screen_idle("Preview · src/")
            # Directory Enter, query paste, file Enter, and editing can arrive
            # together. Each acceptance must resolve before later input runs.
            terminal.send(b"\r\x1b[200~" + "界 target".encode() + b"\x1b[201~\riX")
            terminal.expect_screen_idle("INS")
            terminal.save_from_insert()
            assert target.read_text() == "Xfn destination() {}\n"
            assert origin.read_text() == "fn original() {}\n"
            terminal.send(b"\x0f")
            terminal.expect_screen_idle("fn original() {}")
            terminal.send(b" '")
            terminal.expect_screen_idle("Preview · 界 target.rs")
            terminal.expect_screen_idle("Xfn destination() {}")
            terminal.send(b"\x03")
            terminal.expect_screen_absent("Browse")
            terminal.send(f":browse {root / 'empty directory'}\r".encode())
            terminal.expect_screen_idle("No matching entries")
            terminal.send(b"\x7f")
            terminal.expect_screen_idle("Preview · empty directory/")
            terminal.expect_screen_idle("No visible entries")
            terminal.send(b"\x03")
            terminal.expect_screen_absent("Browse")
            terminal.send(f":browse {root / 'missing'}\r".encode())
            terminal.expect_screen_idle("Cannot browse:")
            terminal.send(b"\x7f")
            terminal.expect_screen_idle("origin.rs")
            terminal.send(b"\x03")
            terminal.expect_screen_absent("Browse")
            terminal.send(b":q!\r")
            terminal.finish()
        print("PASS: directory/file previews, filtering, navigation history, resize, queued enter/edit/save, reopen, paths with spaces, errors, cancellation, terminal cleanup")


if __name__ == "__main__":
    main()
