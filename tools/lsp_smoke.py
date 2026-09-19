#!/usr/bin/env python3
"""Check rust-analyzer integration through the real editor in a Unix PTY.

Run after cargo build --release -p vex_term --locked. Rust-analyzer and a Rust
toolchain must be installed. Protocol and editor unit tests live in Rust sources.
"""

import argparse
import os
from pathlib import Path
import tempfile

from terminal_smoke import Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    parser.add_argument("--server", type=Path)
    args = parser.parse_args()
    binary = str(args.binary.resolve(strict=True))
    if args.server:
        os.environ["VEX_RUST_ANALYZER"] = str(args.server.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="vex-lsp-pty-") as directory:
        project = Path(directory).resolve()
        (project / "Cargo.toml").write_text(
            '[package]\nname = "vex_lsp_pty"\nversion = "0.1.0"\nedition = "2024"\n'
        )
        (project / "src").mkdir()
        main_file = project / "src/main.rs"
        main_file.write_text("mod other;\nfn main() { let _: bool = other::answer(); }\n")
        (project / "src/other.rs").write_text("pub fn answer() -> u32 { 42 }\n")
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(220, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.expect_screen(b"1E")
            terminal.send(b"]d")
            terminal.expect_screen(b"expected")
            terminal.send(b"/answer\r")
            terminal.expect_screen(b"2:39")
            terminal.send(b"K")
            terminal.expect_screen(b"fn answer()")
            terminal.send(b"gd")  # The first key dismisses hover and dispatches normally.
            terminal.expect_screen(b"other.rs")
            terminal.send(b"\x0f")  # Ctrl-o: return to the saved origin.
            terminal.expect_screen(b"main.rs")
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer diagnostics, hover, cross-file definition, jump back, shutdown")

        original = os.environ.get("VEX_RUST_ANALYZER")
        os.environ["VEX_RUST_ANALYZER"] = str(project / "missing-server")
        try:
            with Terminal([binary, str(main_file)]) as terminal:
                terminal.start()
                terminal.expect_screen(b"RA:unavailable")
                terminal.send(b"i// editing works\x03")
                terminal.expect_screen(b"NOR [+]")
                terminal.send(b":q!\r")
                terminal.finish()
        finally:
            if original is None:
                os.environ.pop("VEX_RUST_ANALYZER", None)
            else:
                os.environ["VEX_RUST_ANALYZER"] = original
        print("PASS: missing language server leaves editing and terminal cleanup available")


if __name__ == "__main__":
    main()
