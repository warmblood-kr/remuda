#!/usr/bin/env python3
"""Fail if the landing replay controls regress (remuda#667).

The Play/Pause button's name is its visible text (script-toggled), so it must
not carry a fixed aria-label; replay output must not be a live region, since
it is rewritten every 22ms while animating (the step <output> announces frames).
"""
import re
import sys

html = open("docs/index.html", encoding="utf-8").read()
bad = []
for tag in re.findall(r"<button[^>]*data-action=\"play\"[^>]*>", html):
    if "aria-label" in tag:
        bad.append(f"play button has fixed aria-label: {tag}")
for tag in re.findall(r"<pre[^>]*capture-output[^>]*>", html):
    if "aria-live" in tag:
        bad.append(f"replay output is a live region: {tag}")
if bad:
    sys.exit("\n".join(bad))
