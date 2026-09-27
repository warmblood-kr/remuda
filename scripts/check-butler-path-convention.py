#!/usr/bin/env python3
"""Assert install-butler.sh and daemon.rs agree on the init.lua path.

`docs/install-butler.sh`'s `init_lua="$config_home/remuda/init.lua"` write
target and `native/src/daemon.rs`'s `user_config_path()` -- one shell, one
Rust -- both implement `${XDG_CONFIG_HOME:-$HOME/.config}/remuda/init.lua`.
Edit one side's literal path segment and the other silently keeps its old
value (see steps/034's ceiling comment).

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

for path in (INSTALLER, DAEMON_RS):
    if not path.is_file():
        print(f"missing {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)

installer = INSTALLER.read_text(encoding="utf-8")
daemon_rs = DAEMON_RS.read_text(encoding="utf-8")

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
# Rust side: `PathBuf::from(std::env::var_os("HOME")?).join(".config")`, then
# `config_home.join("remuda").join("init.lua")` in `user_config_path()`.
rust_fallback = re.search(r'var_os\("HOME"\)\?\)\.join\("([^"]*)"\)', daemon_rs)
rust_segments = re.findall(r'config_home\.join\("(\w+)"\)\.join\("([\w.]+)"\)', daemon_rs)

problems2: list[str] = []
if not sh_init_lua_target:
    problems2.append(
        f"{INSTALLER.name}: could not find the init_lua=\"$config_home/remuda/init.lua\" "
        "write target -- parser or convention changed"
    )
if not rust_fallback:
    problems2.append(
        f"{DAEMON_RS.name}: could not find user_config_path()'s HOME fallback "
        "-- parser or convention changed"
    )
if not rust_segments:
    problems2.append(
        f"{DAEMON_RS.name}: could not find user_config_path()'s "
        'config_home.join("remuda").join("init.lua") -- parser or convention changed'
    )

if problems2:
    print(
        "could not extract the init.lua path convention from one or both sides:\n",
        file=sys.stderr,
    )
    for p in problems2:
        print(f"  - {p}", file=sys.stderr)
    sys.exit(1)

rust_fallback_segment = f"/{rust_fallback.group(1)}"
rust_init_lua_segment = "/" + "/".join(rust_segments[0])

if sh_fallback.group(1) != rust_fallback_segment:
    problems2.append(
        f"HOME fallback diverged: install-butler.sh uses $HOME{sh_fallback.group(1)}, "
        f"daemon.rs's user_config_path() uses $HOME{rust_fallback_segment}"
    )
if sh_init_lua_target.group(1) != rust_init_lua_segment:
    problems2.append(
        f"init.lua path diverged: install-butler.sh writes $config_home"
        f"{sh_init_lua_target.group(1)}, daemon.rs reads $config_home{rust_init_lua_segment}"
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
    f"init.lua at $config_home{rust_init_lua_segment}"
)
