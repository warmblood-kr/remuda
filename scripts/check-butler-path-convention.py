#!/usr/bin/env python3
"""Assert install-butler.sh, init.lua, and daemon.rs agree on their path
conventions.

Two independent two-way agreements live in this one file, since both share
`docs/install-butler.sh`'s `config_home` variable and its shell/`$HOME`
fallback shape:

1. `install-butler.sh`'s `config_home`/`token_file`/`config_file` defaults
   and `packages/butler/init.lua`'s `default_config_home()` + `resolve_path()`
   -- one shell, one Lua -- both implement
   `${XDG_CONFIG_HOME:-$HOME/.config}/remuda/butler/{token,config}`.
2. `install-butler.sh`'s new `init_lua="$config_home/remuda/init.lua"` write
   target and `native/src/daemon.rs`'s `user_config_path()` -- one shell, one
   Rust -- both implement `${XDG_CONFIG_HOME:-$HOME/.config}/remuda/init.lua`.

Nothing else compares either pair: edit one side's literal path segment and
the other silently keeps its old value, and nothing fails until someone
notices by eye (see steps/034's ceiling comment, which this check turns from
a hand-checked promise into a real one).

This does not execute any of the three languages against each other -- it
extracts the literal path-segment strings each side hardcodes via regex, and
fails if a pair's segments don't match.

Run it yourself:  python3 scripts/check-butler-path-convention.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INSTALLER = ROOT / "docs" / "install-butler.sh"
INIT_LUA = ROOT / "packages" / "butler" / "init.lua"
DAEMON_RS = ROOT / "native" / "src" / "daemon.rs"

for path in (INSTALLER, INIT_LUA, DAEMON_RS):
    if not path.is_file():
        print(f"missing {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)

installer = INSTALLER.read_text(encoding="utf-8")
init_lua = INIT_LUA.read_text(encoding="utf-8")
daemon_rs = DAEMON_RS.read_text(encoding="utf-8")

# Shell side: config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
sh_fallback = re.search(r'config_home="\$\{XDG_CONFIG_HOME:-\$HOME(/[^}]*)\}"', installer)
# Shell side: every $config_home/remuda/butler/<name> reference (the override
# defaults AND the canonical-copy target both use it -- same literal, so a
# set naturally dedupes them).
sh_segments = set(re.findall(r"\$config_home(/remuda/butler/\w+)\b", installer))

# Lua side: `return home .. "/.config"`, the last line of default_config_home().
lua_fallback = re.search(r'os\.getenv\("HOME"\).*?home \.\. "([^"]*)"', init_lua, re.S)
# Lua side: `path = config_home .. "/remuda/butler/" .. filename` in resolve_path().
lua_join = re.search(r'config_home \.\. "(/remuda/butler/)" \.\. filename', init_lua)
# Lua side: the two resolve_path("REMUDA_BUTLER_...", "<filename>", ...) call sites.
lua_filenames = re.findall(r'resolve_path\("REMUDA_BUTLER_\w+",\s*"(\w+)"', init_lua)

problems: list[str] = []

# A regex that matches nothing must fail rather than pass: silence here means
# the parser broke, not that the two sides quietly agree.
if not sh_fallback:
    problems.append(
        f"{INSTALLER.name}: could not find the XDG_CONFIG_HOME:-$HOME fallback "
        "-- parser or convention changed"
    )
if not sh_segments:
    problems.append(
        f"{INSTALLER.name}: found no $config_home/remuda/butler/<name> segment "
        "-- parser or convention changed"
    )
if not lua_fallback:
    problems.append(
        f"{INIT_LUA.name}: could not find default_config_home()'s HOME fallback "
        "-- parser or convention changed"
    )
if not lua_join:
    problems.append(
        f'{INIT_LUA.name}: could not find resolve_path()\'s '
        'config_home .. "/remuda/butler/" .. filename join -- parser or '
        "convention changed"
    )
if not lua_filenames:
    problems.append(
        f"{INIT_LUA.name}: found no resolve_path(...) call site naming a "
        "filename -- parser or convention changed"
    )

if problems:
    print("could not extract the path convention from one or both sides:\n", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    sys.exit(1)

lua_segments = {lua_join.group(1) + name for name in lua_filenames}

if sh_fallback.group(1) != lua_fallback.group(1):
    problems.append(
        f"HOME fallback diverged: install-butler.sh uses $HOME{sh_fallback.group(1)}, "
        f"init.lua's default_config_home() uses $HOME{lua_fallback.group(1)}"
    )
if sh_segments != lua_segments:
    problems.append(
        f"remuda/butler path segments diverged: install-butler.sh has "
        f"{sorted(sh_segments)}, init.lua has {sorted(lua_segments)}"
    )

if problems:
    print(
        "install-butler.sh and init.lua no longer agree on the butler path convention:\n",
        file=sys.stderr,
    )
    for p in problems:
        print(f"  - {p}", file=sys.stderr)
    print(
        "\ninit.lua's default lookup and install-butler.sh's canonical-copy target "
        "must resolve to the same path, or the daemon will never find what the "
        "installer wrote there -- fix whichever side changed.",
        file=sys.stderr,
    )
    sys.exit(1)

print(
    f"ok — install-butler.sh and init.lua agree: $HOME{sh_fallback.group(1)} "
    f"fallback, segments {sorted(sh_segments)}"
)

# Convention 2: install-butler.sh's `init.lua` write target vs. daemon.rs's
# `user_config_path()`. Reuses `sh_fallback` from above -- one `config_home`
# variable backs both butler's paths and this one, so the fallback is only
# checked once.
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
