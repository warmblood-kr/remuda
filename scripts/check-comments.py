#!/usr/bin/env python3
"""Cap the length of doc comments: 3 lines per item (`///`), 20 per module (`//!`).

정수님, 2026-09-10: *"함수/메소드 주석 최대 3줄 이내. 주로 1-2줄."* and, for the
module header: *"20줄이면 충분하지 않을까요. … 한페이지 넘는 주석은 좀 과도한 것
같고."*

clippy has no line-count lint for either — `too_long_first_doc_paragraph` governs
only the first paragraph — so it is a check here, run in CI, rather than advice.

The module cap is 20 and not the 40 he offered as a ceiling, because 40 was
measured to catch 1 file of 15 while 20 catches 7. A cap that almost nothing
crosses is a description, not a limit.

A block over either cap is usually one of three things, and each has a better
home: history (git log), a rationale (steps/, PRINCIPLES.md), or a proof (a test
name).

Run it yourself:  python3 scripts/check-comments.py
"""

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CAP = 3
MODULE_CAP = 20
SRC = [ROOT / "core" / "src", ROOT / "native" / "src"]


def blocks(lines: list[str]):
    """Yield (start_line, length, following_code) for each run of `///`."""
    i = 0
    while i < len(lines):
        if lines[i].lstrip().startswith("///"):
            j = i
            while j < len(lines) and lines[j].lstrip().startswith("///"):
                j += 1
            following = lines[j].strip() if j < len(lines) else ""
            yield i + 1, j - i, following
            i = j
        else:
            i += 1


problems: list[str] = []
seen = 0

for root in SRC:
    if not root.is_dir():
        print(f"no source tree at {root}", file=sys.stderr)
        sys.exit(1)
    for path in sorted(root.rglob("*.rs")):
        lines = path.read_text(encoding="utf-8").splitlines()
        for start, length, following in blocks(lines):
            seen += 1
            if length > CAP:
                where = f"{path.relative_to(ROOT)}:{start}"
                problems.append(f"{where}  {length} lines  →  {following[:60]}")

        header = sum(1 for l in lines if l.lstrip().startswith("//!"))
        if header > MODULE_CAP:
            problems.append(
                f"{path.relative_to(ROOT)}  module header {header} lines "
                f"(cap {MODULE_CAP})"
            )

# A parser that finds nothing must fail rather than pass: zero blocks means the
# glob or the tree is broken, not that every comment is short.
if seen == 0:
    problems.append("found no `///` blocks at all — the parser or the tree is broken")

if problems:
    print(f"doc comments over cap (item {CAP}, module {MODULE_CAP}):\n", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    print(
        f"\n{len(problems)} over cap. History belongs in git log, rationale in "
        "steps/, proof in a test name. Keep here only what a reader must not miss.",
        file=sys.stderr,
    )
    sys.exit(1)

print(f"ok — {seen} doc comment(s) within cap (item {CAP}, module {MODULE_CAP})")
