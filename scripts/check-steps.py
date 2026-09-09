#!/usr/bin/env python3
"""Verify that every step in `steps/` states its outcome before and after.

정수님, 2026-09-10: *"미리, 현재는 어떠한데 이걸 만들면 어떻게 될거다/고쳐질거다
같은, 일종의 desired outcome을 정의해서 기재해두고, 실제 구현한 다음에는, expected
and actual을 기재합니다."*

A rule you can only remember is a hope, so this is a check rather than a note in
a contributing guide. Four sections, all required:

    ## Before          what is true now, and what is wrong with it
    ## Desired outcome  what will be different once this lands
    ## Expected        what the implementation should produce
    ## Actual          what it produced — VERBATIM, in a fenced block

`## Actual` must contain a fenced code block. That is the one requirement that
cannot be satisfied honestly without having run something: prose can be written
from intention, and captured output cannot. For a terminal tool, pasted output
is what a screenshot is for a UI.

Run it yourself:  python3 scripts/check-steps.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STEPS = ROOT / "steps"

REQUIRED = ["Before", "Desired outcome", "Expected", "Actual"]
# Words that look like an answer and are not one. A step doc filled with these
# passes a "section is non-empty" check while carrying no information.
PLACEHOLDERS = {"", "-", "n/a", "na", "tbd", "todo", "todo.", "pending", "?"}

problems: list[str] = []

if not STEPS.is_dir():
    print(f"no steps/ directory at {STEPS}", file=sys.stderr)
    sys.exit(1)

docs = sorted(STEPS.glob("*.md"))
if not docs:
    problems.append("steps/ has no step documents — the parser or the tree is broken")

for doc in docs:
    text = doc.read_text(encoding="utf-8")
    name = doc.relative_to(ROOT)

    # Split on level-2 headings, keeping each section's body with its title.
    sections = {
        title.strip(): body.strip()
        for title, body in re.findall(
            r"^## (.+?)\n(.*?)(?=\n## |\Z)", text, re.M | re.S
        )
    }

    for required in REQUIRED:
        if required not in sections:
            problems.append(f"{name}: has no `## {required}` section")
            continue
        body = sections[required]
        if body.strip().lower() in PLACEHOLDERS:
            problems.append(f"{name}: `## {required}` is empty or a placeholder")

    actual = sections.get("Actual", "")
    if actual and "```" not in actual:
        problems.append(
            f"{name}: `## Actual` has no fenced block — paste what actually ran. "
            "Prose can be written from intention; captured output cannot."
        )

if problems:
    print("steps/ does not record outcomes the way this repo requires:\n", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    print(
        "\nSee scripts/check-steps.py's docstring for the four required sections.",
        file=sys.stderr,
    )
    sys.exit(1)

print(f"ok — {len(docs)} step(s), each with before, desired, expected and captured actual")
