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

        # Keep the declaration outside the final viewport, so seeing its name
        # verifies the completion list rather than the underlying document.
        source = "/// Returns the completion probe.\nfn vex_completion_target() -> u32 { 42 }\n" + "\n" * 30 + "fn main() { vex_com"
        main_file.write_text(source)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.send(b"gei")
            terminal.expect_screen(f"{len(source.splitlines())}:{len(source.splitlines()[-1]) + 1}".encode())
            # RA:ready means initialized; indexing may still be in progress.
            for attempt in range(3):
                terminal.send(b"\x18")  # Ctrl-x: explicit completion.
                try:
                    terminal.expect_screen(b"vex_completion_target")
                    break
                except AssertionError:
                    if attempt == 2:
                        raise
            terminal.send(b"\t")
            terminal.expect_screen(b"Documentation")
            terminal.expect_screen(b"Returns the completion probe")
            terminal.send(b"\x03")  # Reject completion and stay in insert mode.
            terminal.expect_screen(b"INS")
            terminal.send(b"\x18\t\r\x13")  # Early selection/accept/save await both replies.
            terminal.expect_screen(b"wrote")
            completed = main_file.read_text()
            assert completed.startswith(source.rsplit("vex_com", 1)[0])
            assert "vex_completion_target" in completed.split("fn main()", 1)[1]
            assert "$0" not in completed and "${" not in completed
            terminal.send(b"\x03u\x13")
            terminal.expect_screen(b"fn main() { vex_com ")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == source, main_file.read_text()
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer completion, documentation resolution, rejection, early accept/save, undo, shutdown")

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
