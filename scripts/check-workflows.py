#!/usr/bin/env python3
"""Verify every workflow file still parses and still defines jobs.

The incident, 2026-09-10: a heredoc body written at column 0 inside an indented
`run: |` block ENDS the YAML block scalar it lives in. `ci.yml` stopped parsing,
so GitHub started **zero** jobs — and reported the run as a plain red X, which
is indistinguishable at a glance from a test failure. Every gate in this
repository was silently dead for twelve hours, including `gates-can-fail`, whose
entire job is to notice a gate going quiet.

That is the worst shape a guard can fail in: not wrong, absent. `cargo test`
green and `clippy` green were both true and both irrelevant, because neither ran.

A zero-job run is the tell, and nothing was watching for it. This is.

Run it yourself:  python3 scripts/check-workflows.py
"""

import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"

problems: list[str] = []
files = sorted(WORKFLOWS.glob("*.yml")) + sorted(WORKFLOWS.glob("*.yaml"))

# A glob that finds nothing must fail rather than pass: zero files would
# otherwise mean "every workflow is valid".
if not files:
    print(f"no workflow files under {WORKFLOWS} — the glob or the tree is broken", file=sys.stderr)
    sys.exit(1)

for path in files:
    name = path.relative_to(ROOT)
    try:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
    except yaml.YAMLError as error:
        problems.append(f"{name}: does not parse as YAML — {str(error).splitlines()[0]}")
        continue

    if not isinstance(document, dict):
        problems.append(f"{name}: does not parse as YAML — top level is not a mapping")
        continue

    jobs = document.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        problems.append(f"{name}: defines no jobs — a run of it would do nothing at all")
        continue

    for job, body in jobs.items():
        if not isinstance(body, dict) or not body.get("steps"):
            problems.append(f"{name}: job `{job}` has no steps")

if problems:
    print("a workflow file would not run as written:\n", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    print(
        "\nA workflow that does not parse starts ZERO jobs and still shows a red X. "
        "Every other gate goes quiet with it, and quiet reads exactly like clean.",
        file=sys.stderr,
    )
    sys.exit(1)

total = sum(len(yaml.safe_load(p.read_text(encoding="utf-8"))["jobs"]) for p in files)
print(f"ok — {len(files)} workflow file(s) parse, {total} job(s) defined")
