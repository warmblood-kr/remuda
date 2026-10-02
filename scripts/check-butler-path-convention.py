#!/usr/bin/env python3
"""Assert install-butler.sh and the daemon's shared init.lua resolver agree.

`docs/install-butler.sh` writes `$config_home/remuda/init.lua`, while the
daemon asks the shared storage resolver for the Config directory and appends
`init.lua`. This keeps the Unix installer path aligned while allowing the
resolver to choose the documented Windows location.

The installer-vs-butler-`init.lua` agreement (`remuda/butler/{token,config}`)
used to live here too. Since 356c40f that `init.lua` lives in
warmblood-kr/remuda-butler, so that cross-repo check belongs there.

This extracts the literal path segments each side hardcodes via regex; it
does not execute either language.

Run it yourself:  python3 scripts/check-butler-path-convention.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INSTALLER = ROOT / "docs" / "install-butler.sh"
DAEMON_RS = ROOT / "native" / "src" / "daemon.rs"
STORAGE_RS = ROOT / "native" / "src" / "storage.rs"

for path in (INSTALLER, DAEMON_RS, STORAGE_RS):
    if not path.is_file():
        print(f"missing {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)

installer = INSTALLER.read_text(encoding="utf-8")
daemon_rs = DAEMON_RS.read_text(encoding="utf-8")
storage_rs = STORAGE_RS.read_text(encoding="utf-8")

# Shell side: config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
sh_fallback = re.search(r'config_home="\$\{XDG_CONFIG_HOME:-\$HOME(/[^}]*)\}"', installer)
if not sh_fallback:
    print(
        f"{INSTALLER.name}: could not find the XDG_CONFIG_HOME:-$HOME fallback "
        "-- parser or convention changed",
        file=sys.stderr,
    )
    sys.exit(1)

sh_init_lua_target = re.search(r'init_lua="\$config_home(/remuda/init\.lua)"', installer)
# Rust side: user_config_path selects Config + init.lua, and storage appends
# the resolved per-kind directory plus that file name.
rust_config_subdir = re.search(
    r'fn unix_subdir\(self\)[\s\S]*?Self::Config => "([^"]+)"', storage_rs
)
rust_config_file = re.search(
    r'user_file_path_for\(\s*crate::storage::Kind::Config,\s*"([^"]+)"',
    daemon_rs,
    re.S,
)
rust_resolver = re.search(
    r"fn user_file_path_for\([\s\S]*?resolve_dir_for\(kind\.as_str\(\), windows, env\)"
    r"\s*\.ok\(\)\s*\.map\(\|directory\| directory\.join\(file\)\)",
    storage_rs,
)
rust_remuda_dir = re.search(
    r'base_dir_for\(kind, windows, env\)\?\.join\("remuda"\)', storage_rs
)

problems2: list[str] = []
if not sh_init_lua_target:
    problems2.append(
        f"{INSTALLER.name}: could not find the init_lua=\"$config_home/remuda/init.lua\" "
        "write target -- parser or convention changed"
    )
if not rust_config_file:
    problems2.append(
        f"{DAEMON_RS.name}: could not find user_config_path()'s Config/init.lua "
        "resolver call -- parser or convention changed"
    )
if not rust_config_subdir:
    problems2.append(
        f"{STORAGE_RS.name}: could not find the Config HOME fallback directory "
        "-- parser or convention changed"
    )
if not rust_resolver or not rust_remuda_dir:
    problems2.append(
        f"{STORAGE_RS.name}: could not find the shared resolved Remuda file path "
        "-- parser or convention changed"
    )

if problems2:
    print(
        "could not extract the init.lua path convention from one or both sides:\n",
        file=sys.stderr,
    )
    for p in problems2:
        print(f"  - {p}", file=sys.stderr)
    sys.exit(1)

rust_fallback_segment = f"/{rust_config_subdir.group(1)}"
if sh_fallback.group(1) != rust_fallback_segment:
    problems2.append(
        f"HOME fallback diverged: install-butler.sh uses $HOME{sh_fallback.group(1)}, "
        f"storage Config uses $HOME{rust_fallback_segment}"
    )
if sh_init_lua_target.group(1) != f"/remuda/{rust_config_file.group(1)}":
    problems2.append(
        f"init.lua path diverged: install-butler.sh writes $config_home"
        f"{sh_init_lua_target.group(1)}, daemon.rs resolves $config_home/remuda/"
        f"{rust_config_file.group(1)}"
    )

if problems2:
    print(
        "install-butler.sh and daemon.rs no longer agree on the init.lua path convention:\n",
        file=sys.stderr,
    )
    for p in problems2:
        print(f"  - {p}", file=sys.stderr)
    print(
        "\ndaemon.rs's user_config_path() and install-butler.sh's write target must "
        "resolve to the same path, or a fresh daemon will never read what the "
        "installer wrote there -- fix whichever side changed.",
        file=sys.stderr,
    )
    sys.exit(1)

print(
    f"ok — install-butler.sh and daemon.rs agree: $HOME{rust_fallback_segment} fallback, "
    "init.lua at "
    f"$config_home/remuda/{rust_config_file.group(1)}"
)
