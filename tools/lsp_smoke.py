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
            terminal.send(b" k")
            terminal.expect_screen(b"fn answer()")
            terminal.send(b"gd")  # The first key dismisses hover and dispatches normally.
            terminal.expect_screen(b"other.rs")
            terminal.send(b"\x0f")  # Ctrl-o: return to the saved origin.
            terminal.expect_screen(b"main.rs")
            terminal.expect_screen(b"1E")  # Wait for the reopened workspace to finish indexing/checking.
            terminal.send(b" smain")
            terminal.expect_screen(b"Document symbols")
            terminal.expect_screen(b"main  [function]")
            terminal.send(b"\r")
            terminal.expect_screen(b"2:4")
            terminal.send(b"\x0f")
            terminal.expect_screen(b"2:39")
            terminal.send(b" Sanswer")
            terminal.expect_screen(b"Workspace symbols")
            terminal.expect_screen(b"answer  [function]")
            terminal.send(b"\r")
            terminal.expect_screen(b"other.rs")
            terminal.expect_screen(b"1:8")
            terminal.send(b"\x0f")
            terminal.expect_screen(b"main.rs")
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer diagnostics, hover, definitions, document/workspace symbols, jump back, shutdown")

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
            terminal.send(b"\x18\t\r")  # Early selection/accept await both replies.
            terminal.expect_screen(b"fn main() { vex_completion_target")
            terminal.save_from_insert()
            completed = main_file.read_text()
            assert completed.startswith(source.rsplit("vex_com", 1)[0])
            assert "vex_completion_target" in completed.split("fn main()", 1)[1]
            assert "$0" not in completed and "${" not in completed
            terminal.send(b"u:w\r")
            terminal.expect_screen(b"fn main() { vex_com ")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == source, main_file.read_text()
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer completion, documentation resolution, rejection, early accept/save, undo, shutdown")

        auto_source = (
            "fn vex_completion_target() -> u32 { 42 }\n" + "\n" * 30
            + 'fn main() {\n    let _ = 12345;\n    let _ = "hello";\n}\n'
        )
        main_file.write_text(auto_source)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.send(b"/12345\rcvex_co")
            # Fresh typing retries if rust-analyzer is still indexing. No Ctrl-x
            # or further keypress is needed for the timer to display the menu.
            for attempt in range(3):
                mark = terminal.send(b"m")
                try:
                    terminal.expect(b"Complete", mark)
                    terminal.expect_screen(b"vex_completion_target")
                    break
                except AssertionError:
                    if attempt == 2:
                        raise
                    terminal.send(b"\x7f")
            terminal.send(b"\t\r")
            terminal.expect_screen(b"let _ = vex_completion_target")
            terminal.save_from_insert()
            assert "vex_completion_target" in main_file.read_text().split("fn main()", 1)[1]
            terminal.send(b'/"hello"\ra')
            for attempt in range(3):
                mark = terminal.send(b".")
                try:
                    terminal.expect(b"Complete", mark)
                    break
                except AssertionError:
                    if attempt == 2:
                        raise
                    terminal.send(b"\x7f")
            # The server may rank many methods above len on the first page.
            # Narrow the menu and verify it refreshes after further typing.
            terminal.send(b"le")
            terminal.expect_screen(b" len ")
            terminal.send(b"\x03")  # Reject completion.
            terminal.leave_insert()
            terminal.send(b":q!\r")
            terminal.finish()
        print("PASS: automatic completion after idle, acceptance, and server-triggered member completion")

        original = os.environ.get("VEX_RUST_ANALYZER")
        os.environ["VEX_RUST_ANALYZER"] = str(project / "missing-server")
        try:
            with Terminal([binary, str(main_file)]) as terminal:
                terminal.start()
                terminal.expect_screen(b"RA:unavailable")
                terminal.send(b"i// editing works")
                terminal.leave_insert()
                terminal.expect_screen(b"main.rs [+]")
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
