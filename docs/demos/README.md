# Demo B: live Lua change

Real run of `remuda -s site-demo -e ...` (isolated daemon, stopped afterwards) on remuda 0.1.0-nightly.20261001101519.7f7eac3, macOS, 120x24. Output in the cast is captured from the commands, not typed by hand; the shown commands omit the `-s site-demo` selector. Cast is asciicast v2, rendered with `agg`.

Boundary: the advice sees Lua `remuda.send` calls only; the CLI `remuda send` bypasses it.
Reset: `remuda -s site-demo stop --yes`.
