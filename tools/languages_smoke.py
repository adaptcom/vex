#!/usr/bin/env python3
"""Check bundled language rendering and real language servers in a Unix PTY.

Run after cargo build --release -p vex_term --locked. TypeScript language server
and TypeScript/tsserver must be installed unless --syntax-only is set.
Use --go to also exercise an installed gopls. Unit tests stay in Rust source files.
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
    parser.add_argument("--syntax-only", action="store_true")
    parser.add_argument("--go", action="store_true")
    args = parser.parse_args()
    binary = str(args.binary.resolve(strict=True))
    if args.server:
        args.server.stat()
        os.environ["VEX_TYPESCRIPT_LANGUAGE_SERVER"] = str(args.server.absolute())
    with tempfile.TemporaryDirectory(prefix="vex-languages-pty-") as directory:
        project = Path(directory).resolve()
        (project / "tsconfig.json").write_text('{"compilerOptions":{"strict":true,"noEmit":true}}')
        # Syntax is independent of server installation; also exercise silent
        # missing-server handling for every configured language family.
        syntax_env = {name: str(project / "missing-server") for name in (
            "VEX_MARKSMAN", "VEX_BASH_LANGUAGE_SERVER", "VEX_TYPESCRIPT_LANGUAGE_SERVER",
            "VEX_PYRIGHT", "VEX_GOPLS", "VEX_CLANGD", "VEX_JDTLS", "VEX_CSHARP_LS",
            "VEX_SOURCEKIT_LSP", "VEX_RUBY_LSP", "VEX_INTELEPHENSE", "VEX_LUA_LANGUAGE_SERVER",
            "VEX_HTML_LANGUAGE_SERVER", "VEX_CSS_LANGUAGE_SERVER", "VEX_JSON_LANGUAGE_SERVER",
            "VEX_YAML_LANGUAGE_SERVER", "VEX_TAPLO",
        )}
        for name, source, expected in [
            ("README.md", "# Heading\n\n**bold** and [link](file.md)\n", b"\x1b[38;5;4m"),
            ("script", "#!/usr/bin/env bash\nif true; then echo hello; fi\n", b"\x1b[38;5;1m"),
            ("view.tsx", "const view = <div title=\"hello\" />;\n", b"\x1b[38;5;1m"),
            ("python-script", "#!/usr/bin/env python3.13\ndef greet():\n    return \"hello\"\n", b"\x1b[38;5;1m"),
            ("main.go", "package main\nfunc main() { println(42) }\n", b"\x1b[38;5;1m"),
            ("main.c", "int main(void) { return 42; }\n", b"\x1b[38;5;1m"),
            ("main.C", "class Example { public: int run() { return 42; } };\n", b"\x1b[38;5;1m"),
            ("Main.java", "class Example { String greet() { return \"hello\"; } }\n", b"\x1b[38;5;1m"),
            ("Main.cs", "class Example { int Run() { return 42; } }\n", b"\x1b[38;5;1m"),
            ("main.swift", "func greet() -> String { return \"hello\" }\n", b"\x1b[38;5;1m"),
            ("Gemfile", 'source "https://rubygems.org"\ngem "rake"\n', b"\x1b[38;5;4m"),
            ("index.php", '<?php function greet() { return "hello"; }\n', b"\x1b[38;5;1m"),
            ("lua-script", '#!/usr/bin/env lua5.4\nlocal message = "hello"\n', b"\x1b[38;5;1m"),
            ("index.html", '<div title="hello">text</div>\n', b"\x1b[38;5;2m"),
            ("style.css", '@media screen { body { color: red; } }\n', b"\x1b[38;5;1m"),
            ("package.json", '{"message": "hello", "count": 42}\n', b"\x1b[38;5;4m"),
            ("settings.jsonc", '// comment\n{"message": "hello"}\n', b"\x1b[38;5;4m"),
            ("config.yaml", 'message: "hello"\ncount: 42\n', b"\x1b[38;5;4m"),
            ("Cargo.lock", 'version = 4\nmessage = "hello"\n', b"\x1b[38;5;4m"),
        ]:
            path = project / name
            path.write_text(source)
            with Terminal([binary, str(path)], env=syntax_env, cwd=str(project)) as terminal:
                terminal.start()
                terminal.expect(expected)  # Syntax must wake the otherwise idle terminal.
                terminal.expect_screen_idle("LSP:down")
                terminal.expect_screen_absent("failed to start")
                terminal.send(b":q\r")
                terminal.finish()
        print("PASS: 19 language/detection fixtures render with missing servers and clean shutdown")
        if args.syntax_only:
            return

        if args.go:
            go_project = project / "go-project"
            go_project.mkdir()
            (go_project / "go.mod").write_text("module example.com/vex-smoke\n\ngo 1.18\n")
            path = go_project / "main.go"
            path.write_text("package main\n\nconst answer = 42\n\nfunc main() { println(answer) }\n")
            go_env = {
                "GOCACHE": str(project / "go-cache"),
                "GOMODCACHE": str(project / "go-mod-cache"),
                "GOPLSCACHE": str(project / "gopls-cache"),
                "GOPROXY": "off",
            }
            with Terminal([binary, str(path)], env=go_env, cwd=str(go_project)) as terminal:
                terminal.start()
                terminal.resize(140, 24)
                terminal.expect_screen(b"LSP:ready")
                terminal.send(b"/answer\r k")
                terminal.expect_screen(b"Documentation")
                terminal.expect_screen(b"untyped int")
                terminal.send(b"\x03")
                terminal.expect_screen_absent("Documentation")
                terminal.send(b":q\r")
                terminal.finish()
            print("PASS: real gopls initialization, hover, and shutdown")

        path = project / "main.ts"
        path.write_text('export const person = { name: "Ada", age: 36 };\nconst wrong: number = "oops";\nperson')
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            terminal.resize(140, 24)
            terminal.expect_screen(b"LSP:ready")
            terminal.expect_screen(b"1E")
            terminal.send(b"/wrong\r k")
            terminal.expect_screen(b"Documentation")
            terminal.expect_screen(b"Ctrl-u/Ctrl-d scroll")
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
