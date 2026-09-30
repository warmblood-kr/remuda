#!/usr/bin/env python3
"""Assert the installer offers exactly the platforms the release workflow builds.

The failure this exists for is silent and lands on users, not on us: add a
target to `docs/install.sh` and forget the matrix, and everyone on that platform
gets a 404 from a one-liner the README promised. Reverse the omission and a
platform we do build is unreachable. Nothing else in the build compares the two
files, because they are a shell script and a YAML matrix.

There are TWO installers, because a `uname` case arm cannot run on Windows.
Which one must offer a target is decided by the target triple itself: a
`*-windows-*` triple belongs to `install.ps1` and nothing else, and every other
triple belongs to `install.sh` and nothing else. Splitting the set that way is
what keeps "add a platform, forget the other file" a build failure rather than a
404 — in either direction.

It also checks the shape that makes an asset name: both installers derive
`remuda-<version>-<target>.tar.gz` and the workflow writes it, so the literal is
matched in all three rather than trusted.

Run it yourself:  python3 scripts/check-install.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
INSTALLER = ROOT / "docs" / "install.sh"
INSTALLER_PS1 = ROOT / "docs" / "install.ps1"

problems: list[str] = []

for path in (WORKFLOW, INSTALLER, INSTALLER_PS1):
    if not path.is_file():
        print(f"missing {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)

workflow = WORKFLOW.read_text(encoding="utf-8")
installer = INSTALLER.read_text(encoding="utf-8")
installer_ps1 = INSTALLER_PS1.read_text(encoding="utf-8")

# `- target: <triple>` in the build matrix.
built = set(re.findall(r"^\s*-\s*target:\s*(\S+)\s*$", workflow, re.M))
# `Linux/x86_64) target=<triple> ;;` in the uname case arm.
offered = set(re.findall(r"^\s*\S+/\S+\)\s*target=(\S+)\s*;;", installer, re.M))
# `'X64' = '<triple>'` in the $targets hashtable.
offered_ps1 = set(re.findall(r"^\s*'[^']+'\s*=\s*'(\S+)'\s*$", installer_ps1, re.M))

# A regex that matches nothing must fail rather than pass: an empty set on
# either side would otherwise make the two "agree" perfectly.
if not built:
    problems.append(f"found no `- target:` entries in {WORKFLOW.name} — the parser is broken")
if not offered:
    problems.append(f"found no `target=` case arms in {INSTALLER.name} — the parser is broken")
if not offered_ps1:
    problems.append(f"found no `'arch' = 'triple'` entries in {INSTALLER_PS1.name} — the parser is broken")


def windows(targets: set[str]) -> set[str]:
    return {t for t in targets if "-windows-" in t}


pairs = [
    ("install.sh", offered, built - windows(built)),
    ("install.ps1", offered_ps1, windows(built)),
]
for name, asked, builds in pairs:
    for target in sorted(builds - asked):
        problems.append(f"release.yml builds {target}, but {name} will never ask for it")
    for target in sorted(asked - builds):
        problems.append(
            f"{name} offers {target}, but release.yml does not build it — a 404 for those users"
        )

# The wrong installer offering a target is its own failure: it would run `uname`
# on Windows, or a PowerShell hashtable lookup on Linux.
for target in sorted(windows(offered)):
    problems.append(f"install.sh offers {target} — a Windows triple belongs in install.ps1")
for target in sorted(offered_ps1 - windows(offered_ps1)):
    problems.append(f"install.ps1 offers {target} — a non-Windows triple belongs in install.sh")

# One asset-name shape, spelled the same on every side of the download.
shape = "remuda-$version-$target.tar.gz"
if shape not in installer:
    problems.append(f"install.sh no longer builds the asset name as `{shape}`")
if shape not in installer_ps1:
    problems.append(f"install.ps1 no longer builds the asset name as `{shape}`")
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

print(
    f"ok — {len(built)} target(s) built and offered "
    f"({len(offered)} via install.sh, {len(offered_ps1)} via install.ps1): "
    f"{', '.join(sorted(built))}"
)
