#!/usr/bin/env python3
"""Assert the installer offers exactly the platforms the release workflow builds.

The failure this exists for is silent and lands on users, not on us: add a
target to `docs/install.sh` and forget the matrix, and everyone on that platform
gets a 404 from a one-liner the README promised. Reverse the omission and a
platform we do build is unreachable. Nothing else in the build compares the two
files, because they are a shell script and a YAML matrix.

It also checks the two shapes that make an asset name: `install.sh` derives
`remuda-<version>-<target>.tar.gz` and the workflow writes it, so the literal is
matched in both rather than trusted.

Run it yourself:  python3 scripts/check-install.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
INSTALLER = ROOT / "docs" / "install.sh"

problems: list[str] = []

for path in (WORKFLOW, INSTALLER):
    if not path.is_file():
        print(f"missing {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)

workflow = WORKFLOW.read_text(encoding="utf-8")
installer = INSTALLER.read_text(encoding="utf-8")

# `- target: <triple>` in the build matrix.
built = set(re.findall(r"^\s*-\s*target:\s*(\S+)\s*$", workflow, re.M))
# `Linux/x86_64) target=<triple> ;;` in the uname case arm.
offered = set(re.findall(r"^\s*\S+/\S+\)\s*target=(\S+)\s*;;", installer, re.M))

# A regex that matches nothing must fail rather than pass: an empty set on
# either side would otherwise make the two "agree" perfectly.
if not built:
    problems.append(f"found no `- target:` entries in {WORKFLOW.name} — the parser is broken")
if not offered:
    problems.append(f"found no `target=` case arms in {INSTALLER.name} — the parser is broken")

for target in sorted(built - offered):
    problems.append(f"release.yml builds {target}, but install.sh will never ask for it")
for target in sorted(offered - built):
    problems.append(f"install.sh offers {target}, but release.yml does not build it — a 404 for those users")

# One asset-name shape, spelled the same on both sides of the download.
shape = "remuda-$version-$target.tar.gz"
if shape not in installer:
    problems.append(f"install.sh no longer builds the asset name as `{shape}`")
if "remuda-${{ needs.plan.outputs.version }}-${{ matrix.target }}" not in workflow:
    problems.append("release.yml no longer names assets remuda-<version>-<target>")

# `dist::is_newer` sorts the prerelease lexically and a sha has no order, so
# the stamp must reach the second: with `%Y%m%d` two nightlies of one day rank
# by sha, and the binary really did offer an older build as an upgrade.
if not re.search(r"nightly\.\$\(date -u \+%Y%m%d%H%M%S\)", workflow):
    problems.append(
        "release.yml's nightly stamp is not to the second — two nightlies of one "
        "day would rank by git sha, which has no order (see dist.rs's tests)"
    )

if problems:
    print("the installer and the release workflow disagree:\n", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    print(
        "\nA platform in one file and not the other is a broken one-liner in the "
        "README — the only place it shows up is a user's terminal.",
        file=sys.stderr,
    )
    sys.exit(1)

print(f"ok — {len(built)} target(s) built and offered: {', '.join(sorted(built))}")
