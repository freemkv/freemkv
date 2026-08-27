#!/usr/bin/env python3
"""Fail if any inline `//` comment block runs longer than MAX_LINES.

Rustdoc (`///`, `//!`) is exempt — long API docs are fine. Only consecutive
non-doc `//` lines are counted; a blank line, code, or a doc line ends the run.
Block comments (`/* */`) are not inspected. Scans src/ and tests/ under the cwd.
"""
import sys
from pathlib import Path

MAX_LINES = 3


def is_inline_comment(line: str) -> bool:
    s = line.lstrip()
    return s.startswith("//") and not s.startswith("///") and not s.startswith("//!")


def violations(path: Path):
    """Yield (start_line, length) for each inline run longer than MAX_LINES."""
    out = []
    run_start = None
    run_len = 0
    for i, line in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        if is_inline_comment(line):
            if run_start is None:
                run_start = i
            run_len += 1
        else:
            if run_len > MAX_LINES:
                out.append((run_start, run_len))
            run_start, run_len = None, 0
    if run_len > MAX_LINES:
        out.append((run_start, run_len))
    return out


def main(argv):
    root = Path(argv[1]) if len(argv) > 1 else Path.cwd()
    files = sorted(p for d in ("src", "tests") for p in (root / d).rglob("*.rs"))
    total = 0
    for f in files:
        for start, length in violations(f):
            rel = f.relative_to(root)
            print(f"{rel}:{start}: inline comment block is {length} lines (max {MAX_LINES})")
            total += 1
    if total:
        print(f"\ncomment-guard: {total} inline comment block(s) exceed {MAX_LINES} lines.")
        print("Trim to the essential why, or split the code so the comment isn't needed.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
