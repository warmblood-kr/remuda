#!/usr/bin/env python3
"""Advance a channel in latest.json only when the candidate is newer."""

import datetime
import json
import pathlib
import sys


def rank(version):
    triple, _, prerelease = version.partition("-")
    numbers = []
    for field in triple.split(".")[:3]:
        if not field.isascii() or not field.isdigit():
            numbers.append(0)
            continue
        value = int(field)
        numbers.append(value if value <= 2**64 - 1 else 0)
    numbers.extend([0] * (3 - len(numbers)))
    return (*numbers, not prerelease, prerelease)


def is_newer(candidate, current):
    """Match native/src/dist.rs rank ordering for channel versions."""
    return rank(candidate) > rank(current)


def advance(path, channel, candidate):
    index = json.loads(path.read_text())
    current = index.get(channel, "")
    if candidate == current:
        print(f"latest.json {channel} already points to {candidate}", file=sys.stderr)
        return None
    if not is_newer(candidate, current):
        print(
            f"latest.json {channel} is already {current}; "
            f"skipping stale candidate {candidate}",
            file=sys.stderr,
        )
        return False

    index[channel] = candidate
    index["updated"] = datetime.datetime.now(datetime.timezone.utc).isoformat(
        timespec="seconds"
    ).replace("+00:00", "Z")
    path.write_text(json.dumps(index, indent=2) + "\n")
    return True


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit("usage: latest-index.py PATH CHANNEL VERSION")
    result = advance(pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3])
    if result is None:
        print("publish=already")
    elif result:
        print("publish=true")
    else:
        print("publish=false")
