#!/usr/bin/env python3
"""Assert install-butler.sh and init.lua agree on the butler path convention.

`docs/install-butler.sh`'s `config_home`/`token_file`/`config_file` defaults
and `packages/butler/init.lua`'s `default_config_home()` + `resolve_path()`
are two independent implementations of one convention --
`${XDG_CONFIG_HOME:-$HOME/.config}/remuda/butler/{token,config}` -- one shell,
one Lua. Nothing else compares them: edit one side's literal path segment and
the other silently keeps its old value, and nothing fails until someone
notices by eye (see steps/034's ceiling comment, which this check turns from
a hand-checked promise into a real one).

This does not execute either language against the other -- it extracts the
literal path-segment strings each side hardcodes (the `$HOME` fallback
suffix, and the `remuda/butler/{token,config}` join) via regex, and fails if
either side's segments don't match the other's.

Run it yourself:  python3 scripts/check-butler-path-convention.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INSTALLER = ROOT / "docs" / "install-butler.sh"
INIT_LUA = ROOT / "packages" / "butler" / "init.lua"

for path in (INSTALLER, INIT_LUA):
    if not path.is_file():
        print(f"missing {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)

installer = INSTALLER.read_text(encoding="utf-8")
init_lua = INIT_LUA.read_text(encoding="utf-8")

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
