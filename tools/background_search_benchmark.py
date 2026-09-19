#!/usr/bin/env python3
"""Measure PTY resize and search cancellation while a large search is pending."""

import argparse
import json
import statistics
import tempfile
import time
from pathlib import Path

from terminal_smoke import Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/vex"))
    parser.add_argument("--mib", type=int, default=100)
    parser.add_argument("--runs", type=int, default=5)
    args = parser.parse_args()
    if args.mib < 16 or args.runs < 1:
        parser.error("use at least 16 MiB and one run")
    binary = str(args.binary.resolve(strict=True))
    samples = []
    with tempfile.TemporaryDirectory(prefix="vex-background-") as directory:
        path = Path(directory) / "large.txt"
        chunk = (b"abcdefghijklmnopqrstuvwxyz0123456789\n" * 30000)[:1 << 20]
        with path.open("wb") as file:
            for _ in range(args.mib):
                file.write(chunk)
        with Terminal([binary, str(path)]) as terminal:
            terminal.start()
            for run in range(args.runs):
                mark = terminal.send(b"/\x1b[200~___absent___\x1b[201~\x1b[I")
                terminal.expect(b"searching...", mark)
                start = time.perf_counter()
                terminal.resize(81 + run % 2, 24)
                resize = (time.perf_counter() - start) * 1000
                start = time.perf_counter()
                mark = terminal.send(b"\x03")  # Ctrl-c cancels the prompt.
                # The document's block cursor replaces the prompt's bar.
                terminal.expect(b"\x1b[2 q", mark)
                cancel = (time.perf_counter() - start) * 1000
                samples.append({"resize_ms": resize, "cancel_ms": cancel})
            terminal.send(b"\x11")  # Clean quit also joins both producer threads.
            terminal.finish()
    print(json.dumps({
        "mib": args.mib,
        "runs": args.runs,
        "median_resize_ms": statistics.median(s["resize_ms"] for s in samples),
        "median_cancel_ms": statistics.median(s["cancel_ms"] for s in samples),
        "samples": samples,
    }, indent=2))


if __name__ == "__main__":
    main()
