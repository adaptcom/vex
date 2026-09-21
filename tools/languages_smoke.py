#!/usr/bin/env python3
"""Check bundled language rendering and the real TypeScript server in a Unix PTY.

Run after cargo build --release -p vex_term --locked. TypeScript language server
and TypeScript/tsserver must be installed. Unit tests stay in Rust source files.
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
        args.server.stat()
        os.environ["VEX_TYPESCRIPT_LANGUAGE_SERVER"] = str(args.server.absolute())
    with tempfile.TemporaryDirectory(prefix="vex-languages-pty-") as directory:
        project = Path(directory).resolve()
        (project / "tsconfig.json").write_text('{"compilerOptions":{"strict":true,"noEmit":true}}')
        for name, source, expected in [
            ("README.md", "# Heading\n\n**bold** and [link](file.md)\n", b"\x1b[38;5;4m"),
            ("script", "#!/usr/bin/env bash\nif true; then echo hello; fi\n", b"\x1b[38;5;1m"),
            ("view.tsx", "const view = <div title=\"hello\" />;\n", b"\x1b[38;5;1m"),
        ]:
            path = project / name
            path.write_text(source)
            with Terminal([binary, str(path)]) as terminal:
                terminal.start()
                terminal.expect(expected)  # Syntax must wake the otherwise idle terminal.
                terminal.send(b":q\r")
                terminal.finish()
        print("PASS: Markdown, shell shebang, and TSX syntax with terminal cleanup")

        path = project / "main.ts"
        path.write_text('export const person = { name: "Ada", age: 36 };\nconst wrong: number = "oops";\nperson')
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"TS:ready")
            terminal.expect_screen(b"1E")
            terminal.send(b"/wrong\r k")
            terminal.expect_screen(b"Documentation")
            terminal.expect_screen(b"Esc closes")
            terminal.send(b'/"oops"\rc1')
            terminal.save_from_insert()
            terminal.expect_screen(b"0E 0W")
            terminal.send(b"gei")
            mark = terminal.send(b".")
            terminal.expect(b"Complete", mark)
            terminal.send(b"na")
            terminal.expect_screen(b" name ")
            terminal.send(b"\t\r")
            terminal.expect_screen(b"person.name")
            terminal.save_from_insert()
            assert path.read_text().endswith("person.name"), path.read_text()
            terminal.send(b":q\r")
            terminal.finish()
        print("PASS: real TypeScript diagnostics, hover, edits, automatic completion, acceptance, save, shutdown")


if __name__ == "__main__":
    main()
