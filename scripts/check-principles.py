#!/usr/bin/env python3
"""Verify that PRINCIPLES.md describes machines that actually exist.

A principles document decays in one direction: a CI job gets renamed, a lint key
gets dropped, a test gets deleted — and the document keeps asserting the rule is
enforced. Nothing about that is visible on the page. This turns that silent decay
into a build failure.

Run it yourself:  python3 scripts/check-principles.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PRINCIPLES = ROOT / "PRINCIPLES.md"
CI = ROOT / ".github/workflows/ci.yml"
CLIPPY = ROOT / "core/clippy.toml"

# The document must keep growing only with real principles; a parser that finds
# nothing must fail rather than pass. See PRINCIPLES.md §2.
MIN_PRINCIPLES = 8

problems: list[str] = []


def require(path: Path) -> str:
    if not path.is_file():
        problems.append(f"missing file: {path.relative_to(ROOT)}")
        return ""
    return path.read_text(encoding="utf-8")


doc = require(PRINCIPLES)
ci = require(CI)
clippy = require(CLIPPY)

headings = re.findall(r"^## \d+\. (.+)$", doc, re.M)

# Read the WHOLE field, not its first line. An `Enforced by:` entry naming two
# mechanisms wraps, and a `$`-anchored pattern silently reads only the head of
# it — measured 2026-09-10, when a deliberately broken test name on the second
# line passed this very check. Multi-line fields read as single-line ones is a
# defect this project hit four times in one night; here it was inside the tool
# built to catch defects.
enforced = [
    " ".join(block.split())
    for block in re.findall(
        r"^\*\*Enforced by:\*\* (.+?)(?=\n\n|\n## |\Z)", doc, re.M | re.S
    )
]

if len(headings) < MIN_PRINCIPLES:
    problems.append(
        f"found {len(headings)} numbered principles, expected at least "
        f"{MIN_PRINCIPLES} — the parser or the document is broken"
    )
if len(headings) != len(enforced):
    problems.append(
        f"{len(headings)} principles but {len(enforced)} 'Enforced by:' lines — "
        "every principle needs one, even if it says 'nothing — discipline only'"
    )

# Only names under `jobs:` count. Scanning the whole file also matches `push:`
# under `on:`, which indents identically — measured 2026-09-10, and it would
# have let `CI job \`push\`` pass as a real mechanism. Fail closed if the
# section is missing rather than silently finding no jobs.
if ci and "\njobs:\n" not in ci:
    problems.append("ci.yml has no `jobs:` section — cannot verify any CI job name")
jobs_section = ci.split("\njobs:\n", 1)[1] if "\njobs:\n" in ci else ""
ci_jobs = set(re.findall(r"^  ([a-z][a-z0-9-]*):$", jobs_section, re.M))
clippy_keys = set(re.findall(r"^([a-z][a-z0-9-]*)\s*=", clippy, re.M))
clippy_paths = set(re.findall(r'path\s*=\s*"([^"]+)"', clippy))

test_sources = "\n".join(
    p.read_text(encoding="utf-8")
    for p in sorted(ROOT.glob("*/tests/*.rs")) + sorted(ROOT.glob("*/src/*.rs"))
)
test_fns = set(re.findall(r"^\s*fn ([a-z_][a-z0-9_]*)\(", test_sources, re.M))

for line in enforced:
    if line.startswith("nothing — discipline only"):
        continue  # An honest blank is a valid answer; a promise is not.

    # Classify every backticked token BY SHAPE, not by the prose around it.
    #
    # The first version matched `test \`x\`` — the word "test" followed by a
    # name. Measured 2026-09-10: the second test on the same line is introduced
    # as "and its negative control \`y\`", so it was never checked, and a
    # deliberately broken name there passed. A checker keyed to how a sentence
    # is phrased goes quiet the moment someone rephrases it.
    #
    # An unrecognised token is an ERROR, not a skip: a mechanism this script
    # cannot classify is a mechanism it cannot verify, and silently ignoring it
    # is exactly the fail-open this file exists to prevent.
    for token in re.findall(r"`([^`]+)`", line):
        if token.startswith("std::"):
            if token not in clippy_paths:
                problems.append(f"says `{token}` is denied, but clippy.toml does not deny it")
        elif token.endswith("-threshold"):
            if token not in clippy_keys:
                problems.append(f"names clippy key `{token}`, absent from clippy.toml")
        elif "/" in token or token.endswith(".toml"):
            if not (ROOT / token).exists():
                problems.append(f"names file `{token}`, which does not exist")
        elif "_" in token and re.fullmatch(r"[a-z0-9_]+", token):  # snake_case
            if token not in test_fns:
                problems.append(f"names test `{token}`, which no source file defines")
        elif re.fullmatch(r"[a-z0-9-]+", token):  # kebab-case, or a bare word: CI job
            if token not in ci_jobs:
                problems.append(f"names CI job `{token}`, which ci.yml does not define")
        else:
            problems.append(
                f"names `{token}`, which this script cannot classify — "
                "an unverifiable mechanism is not a mechanism"
            )

if problems:
    print("PRINCIPLES.md describes machines that do not exist:\n", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    print(
        "\nEither restore the mechanism or change the line to "
        "'nothing — discipline only'.",
        file=sys.stderr,
    )
    sys.exit(1)

print(
    f"ok — {len(headings)} principles, every named mechanism exists "
    f"({len(ci_jobs)} CI jobs, {len(clippy_paths)} denied paths, {len(test_fns)} test fns seen)"
)
