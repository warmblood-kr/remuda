#!/usr/bin/env python3
"""Check production features separately from test-support's feature unification."""
import subprocess
from pathlib import Path

manifest = Path(__file__).resolve().parents[1] / "Cargo.toml"
result = subprocess.run(
    ["cargo", "tree", "--offline", "--locked", "--manifest-path", str(manifest),
     "-e", "normal,features", "-i", "zed-font-kit"],
    check=True, capture_output=True, text=True,
)
assert 'gpui feature "font-kit"' in result.stdout, result.stdout
print("production GPUI font-kit feature enabled")
