#!/bin/sh
# Fail if the landing replay controls regress (remuda#667).
# The Play/Pause button is named by its script-toggled visible text, so it must
# not carry a fixed aria-label; the animated replay output must not be a live
# region (the step <output> announces frames instead).
set -eu
cd "$(dirname "$0")/.."
f=docs/index.html
bad=$(grep -E 'data-action="play"[^>]*aria-label|<pre[^>]*capture-output[^>]*aria-live' "$f" || true)
[ -z "$bad" ] || { printf 'check-landing-a11y: %s\n' "$bad" >&2; exit 1; }
