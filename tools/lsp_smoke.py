#!/usr/bin/env python3
"""Check rust-analyzer integration through the real editor in a Unix PTY.

Run after cargo build --release -p vex_term --locked. Rust-analyzer and a Rust
toolchain must be installed. Protocol and editor unit tests live in Rust sources.
"""

import argparse
import os
from pathlib import Path
import re
import tempfile

from terminal_smoke import Terminal, displayed_text


def empty_diagnostic_pickers(binary, directory):
    source = directory / "notes.txt"
    source.write_text("No language server needed for an empty diagnostic catalog.\n")
    for binding in (b" d", b" D"):
        with Terminal([binary, str(source)]) as terminal:
            terminal.start()
            terminal.send(binding)
            # Do not use expect_screen here: its focus events force a repaint
            # and hide a missing redraw after the worker delivers zero rows.
            terminal.expect_screen_idle("No matching diagnostics")
            terminal.send(b"\x03:q\r")
            terminal.finish()
    print("PASS: empty document/workspace diagnostics repaint without further input")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    parser.add_argument("--server", type=Path)
    args = parser.parse_args()
    binary = str(args.binary.resolve(strict=True))
    if args.server:
        # Rustup chooses its tool from argv[0]; resolving this symlink would
        # invoke rustup itself instead of rust-analyzer.
        args.server.stat()
        os.environ["VEX_RUST_ANALYZER"] = str(args.server.absolute())
    with tempfile.TemporaryDirectory(prefix="vex-lsp-pty-") as directory:
        project = Path(directory).resolve()
        empty_diagnostic_pickers(binary, project)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "vex_lsp_pty"\nversion = "0.1.0"\nedition = "2024"\n'
        )
        (project / "src").mkdir()
        main_file = project / "src/main.rs"
        main_file.write_text(
            "/// Add **two** values.\n"
            "fn add(left: u32, right: u32) -> u32 { left + right }\n"
            "fn main() { let _ = add(1, 2); }\n"
        )
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"RA:ready")
            # Restore the argument position on each indexing retry. Escape from
            # insert mode moves the cursor left, outside the call on later tries.
            for attempt in range(3):
                terminal.send(b"/add\\(1\r;i")
                try:
                    terminal.expect_screen_idle("Signature")
                    terminal.expect_screen_idle("left: u32")
                    break
                except AssertionError:
                    if attempt == 2:
                        raise
                    terminal.leave_insert()
            assert b"\x1b[48;5;7mleft: u32" in terminal.output
            mark = terminal.send(b"1,")
            terminal.expect(b"\x1b[48;5;7mright: u32", mark)
            terminal.leave_insert()
            terminal.send(b":q!\r")
            terminal.finish()
        print("PASS: automatic rust-analyzer signature help and active parameter updates")

        main_file.write_text(
            "struct Demo { member: u32 }\n"
            "impl Demo { fn method(&self) -> u32 { self.member } }\n"
            "fn main() { let demo = Demo { member: 1 }; demo. }\n"
        )
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.send(b"/demo\\.\ra")
            for attempt in range(3):
                terminal.send(b"\x18")
                try:
                    terminal.expect_screen_idle("field")
                    visible = displayed_text(terminal.output)
                    assert re.search(r"member[^\n]*\bfield\b", visible), visible
                    assert re.search(r"method[^\n]*\bmethod\b", visible), visible
                    break
                except AssertionError:
                    if attempt == 2:
                        raise
            terminal.send(b"\x03")
            terminal.leave_insert()
            terminal.send(b":q!\r")
            terminal.finish()
        print("PASS: completion menu displays rust-analyzer field and method kinds")

        main_file.write_text("mod other;\nfn main() { let _: bool = other::answer(); }\n")
        (project / "src/other.rs").write_text("pub fn answer() -> u32 { 42 }\n")
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(220, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.expect_screen(b"1E")
            terminal.send(b" d")
            terminal.expect_screen(b"Document diagnostics")
            terminal.expect_screen(b"expected")
            terminal.send(b"\r")
            terminal.send(b"]d")
            terminal.expect_screen(b"expected")
            # Picker jumps deliberately leave a backwards range. Establish a
            # fresh forward selection before checking search's cursor column.
            terminal.send(b"gg/answer\r")
            terminal.expect_screen(b"2:39")
            terminal.send(b" k")
            terminal.expect_screen(b"fn answer()")
            terminal.send(b"gd")  # The first key dismisses hover and dispatches normally.
            terminal.expect_screen(b"other.rs")
            terminal.expect_screen(b"1:8")  # The function name, not `pub fn`.
            terminal.send(b" D")
            terminal.expect_screen(b"Workspace diagnostics")
            terminal.expect_screen(b"main.rs:")
            terminal.send(b"\x03")  # Close the picker without altering jump history.
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
        print("PASS: real rust-analyzer diagnostics, document/workspace diagnostic pickers, hover, definitions, symbols, jump back, shutdown")

        # Markdown is prepared by the service and shown in a floating, scrollable box.
        markdown_docs = (
            "/// # Hover heading\n///\n"
            "/// **Bold** and `inline code` with a [link](https://example.com).\n///\n"
            + "".join(f"/// Paragraph {index} in the documentation.\n///\n" for index in range(25))
            + "/// Final hover paragraph.\n"
            "fn documented() -> u32 { 42 }\n"
            "fn main() { let unused = documented(); }\n"
        )
        main_file.write_text(markdown_docs)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(120, 30)
            terminal.expect_screen(b"RA:ready")
            terminal.expect_screen(b"1W")
            terminal.send(b"/documented\r")
            terminal.send(b" k")
            terminal.expect_screen(b"Documentation")
            terminal.expect_screen("│ Hover heading".encode())
            terminal.expect_screen(b"Bold and inline code with a link.")
            terminal.send(b"\x04" * 12)
            terminal.expect_screen("│ Final hover paragraph.".encode())
            terminal.send(b"\x15" * 12)
            terminal.expect_screen("│ Hover heading".encode())
            terminal.send(b"\x03:q\r")
            terminal.finish()
        print("PASS: Markdown hover, floating border, wrapping, half-page scrolling and dismissal")

        # A diagnostic quick fix exercises the code-action menu with a real server.
        action_source = "fn main() { let unused = 42; }\n"
        main_file.write_text(action_source)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.expect_screen(b"1W")
            terminal.send(b"]d a")
            terminal.expect_screen(b"Code actions")
            terminal.expect_screen(b"_unused")
            terminal.send(b"\r")
            terminal.expect_screen(b"let _unused = 42")
            assert main_file.read_text() == action_source
            terminal.send(b":w\r")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == action_source.replace("unused", "_unused")
            terminal.send(b"u:w\r")
            terminal.expect_screen(b"let unused = 42")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == action_source
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer code actions, diagnostic quick fix, unsaved edits, explicit save and undo")

        # Stable rustfmt supports whole files; range formatting is opt-in.
        format_source = "fn main(){let _x=1;}\n"
        formatted = "fn main() {\n    let _x = 1;\n}\n"
        main_file.write_text(format_source)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.send(b"=")
            terminal.expect_screen(b"range formatting")
            assert main_file.read_text() == format_source
            # An early write must wait for the formatting request and edit worker.
            terminal.send(b":fmt\r:w\r")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == formatted, main_file.read_text()
            terminal.send(b"u:w\r")
            terminal.expect_screen(b"fn main(){let _x=1;}")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == format_source
            terminal.send(b"U:format\r")
            terminal.expect_screen(b"no formatting changes")
            terminal.send(b":wq\r")
            terminal.finish()
            assert main_file.read_text() == formatted
        print("PASS: real rust-analyzer document formatting, range capability check, early write ordering, undo/redo and no-op formatting")

        # Navigation uses ranges and document highlights from the real server.
        navigation_source = (
            "struct Widget;\n"
            "trait Measure {}\n"
            "impl Measure for Widget {}\n"
            "fn main() {\n"
            "    let value = Widget;\n"
            "    let _ = (&value, &value);\n"
            "}\n"
        )
        main_file.write_text(navigation_source)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(180, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.expect_screen(b"1W")  # Workspace loading and cargo check finished.
            terminal.send(b"/value\rgy")
            terminal.expect_screen(b"1:1")
            terminal.send(b"\x0f")
            terminal.expect_screen(b"5:13")
            terminal.send(b"gr")
            terminal.expect_screen(b"References")
            terminal.expect_screen(b"3 locations")
            terminal.send(b"\x1b")
            terminal.expect_screen(b"NOR")
            terminal.send(b" h")
            terminal.expect_screen(b"3 sel")
            terminal.send(b"citem")
            terminal.save_from_insert()
            assert main_file.read_text() == navigation_source.replace("value", "item")
            terminal.send(b",gg/Measure\rgi")
            terminal.expect_screen(b"3:1")
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer references picker, type/implementation ranges, document reference selections and edits")

        # Rename must use the unsaved declaration in a hidden buffer, while
        # leaving disk writes and each buffer's undo history under user control.
        rename_source = "mod other;\nfn main() { let _: bool = other::answer(); }\n"
        declaration = "pub fn answer() -> u32 { 42 }\n"
        other_file = project / "src/other.rs"
        main_file.write_text(rename_source)
        other_file.write_text(declaration)
        with Terminal([binary, str(main_file)]) as terminal:
            terminal.start()
            terminal.resize(180, 24)
            terminal.expect_screen(b"RA:ready")
            terminal.expect_screen(b"1E")
            terminal.send(b":e " + os.fsencode(other_file) + b"\r")
            terminal.expect_screen(b"other.rs")
            terminal.send(b"ggO// unsaved")
            terminal.expect_screen(b"INS")
            terminal.send(b"\x1b")
            terminal.expect_screen(b"NOR")
            terminal.send(b":e " + os.fsencode(main_file) + b"\r")
            terminal.expect_screen(b"main.rs")
            terminal.expect_screen(b"1E")
            terminal.send(b"/answer\r r")
            terminal.expect_screen(b"rename-to:answer")
            terminal.send(b"\x15result\r")  # Ctrl-u replaces the prefilled name.
            terminal.expect_screen(b"updated 2 buffer(s)")
            assert main_file.read_text() == rename_source
            assert other_file.read_text() == declaration
            terminal.send(b"gd")  # Same server must now know the hidden new name.
            terminal.expect_screen(b"other.rs")
            terminal.expect_screen(b"pub fn result")
            terminal.expect_screen(b"// unsaved")
            terminal.send(b"\x0f")
            terminal.expect_screen(b"main.rs")
            terminal.send(b":w\r")
            terminal.expect_screen(b"wrote")
            assert main_file.read_text() == rename_source.replace("answer", "result")
            terminal.send(b":e " + os.fsencode(other_file) + b"\r")
            terminal.expect_screen(b"pub fn result")
            terminal.expect_screen(b"// unsaved")
            terminal.send(b"u:w\r")
            terminal.expect_screen(b"wrote")
            assert other_file.read_text() == "// unsaved\n" + declaration
            terminal.send(b"U:w\r")
            terminal.expect_screen(b"pub fn result")
            terminal.expect_screen(b"wrote")
            assert other_file.read_text() == "// unsaved\n" + declaration.replace("answer", "result")
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real rust-analyzer rename prompt, hidden unsaved buffer synchronization, per-buffer undo/redo and explicit saves")

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
