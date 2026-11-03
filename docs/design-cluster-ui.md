# Frontend Lua cluster composition contract

2026-11-03 joint plan; base 7e85321;
feat/cluster-lua-composition. No daemon/protocol/Eval changes.

## Scope/loading
Lua owns rows, colors, order, indentation, panel extent and split axis. Rust
supplies facts, validates primitives, renders cells and dispatches actions.
Cluster tree and whole-node RemotePort support both axes; existing list/preview
only. Local herd unchanged; peer has_lua=false. No Image/tools.lua/init.lua.
Frontend cluster_ui.rs/cluster_ui.lua loads Config/cluster-ui.lua once/open
via storage Config resolution; UTF-8 <=1 MiB, reopen to reload. Embedded default
is callable by overrides. Rust Composer/Model/Composition test seam.

## Schema v1
Input: surface=cluster|peer, viewport={cols,rows,body_rows}, selected token,
counts={reachable,total}, groups=[{id,kind=local|node|peer,selected,expanded,
label,state,age_seconds,pending,status,status_cells,sessions=[{id,label,selected,
alive,pending,status,status_cells}]}]. Visibility/drag are input facts. Rust
prefilters search/attention/expansion. Tokens hide routes; no addresses/secrets,
PTY/screens/commands/env/paths/callables. Error context stays safe Rust chrome.

Output:
```
{version=1,title={Row,...},empty={Row,...},
 groups={{id=token,rows={Row,...},
          sessions={{id=token,rows={Row,...}},...}},...},
 layout={axis="horizontal"|"vertical",list_cells=integer,divider="│"}}
Row = {spans={Span,...}}
Span = {text=string OR field="status",fg=Color?,bg=Color?,
        bold=boolean?,dim=boolean?,reverse=boolean?,italic=boolean?,underline=boolean?}
Color = {index=0..255} OR {rgb={r=0..255,g=0..255,b=0..255}}
```
Each group/session appears once under its parent; multirow blocks map to one
target. Node groups/sessions navigate; local/peer headings/title/empty do not.
Group/session order drives Up/Down. Require one field=status per operational
target: Rust substitutes facts; Lua owns placement/style. Cluster defaults retain
reachability/age/live/ended/pending/error information. Peer default status may
be empty: plain names, daemon order, reverse selection, no added gap/caret;
truth remains native chrome. Visibility is user toggle/keys mode, not output.

## Geometry/identity
list_cells excludes divider: columns horizontal, rows vertical. Lua default
owns 60 threshold and 24..48 <= half content-fit/stacking policy. Rust checks
finite extents, bounds/nonoverlap, reserved chrome and feasible one-cell leaves;
tiny/zero content degrades safely. Divider width=1, glyph from Lua.
ONE geometry resolves before resize: list/preview/divider Rect{x,y,w,h},
zero-based half-open. Paint/capture/resize/caret/mouse share it. Keys fullscreen.
Peer rectangle painter uses preview.h for crop/history; caret adds preview.x/y
and terminal +1 once. Mouse translates to local rect; list scroll uses list.h;
drag axis coordinate -> preferred_list_cells. Visual-selection helpers agree.
Resize-stale geometry forces recompose before consumers; no silent hidden list.

Publish composition/routes atomically with generation. Routes use local
id+instance or registry+wire name+instance, never label/index. Lost/replaced
targets cancel stale actions/confirmations; missing legacy IDs get conservative
generation identity. No stale maps or Lua callbacks.

## Safety/fallback/envelope
Only base/table/string/math/utf8. Remove load/loadfile/dofile, print/warn,
pcall/xpcall, string find/match/gmatch/gsub; no io/os/package/debug/require or
process/network APIs. Exhaustion latches; helpers/decode check budgets. Reject
wrong types/version, metatables/sparse tables, missing/duplicate/unknown targets,
invalid hierarchy/styles atomically. Strip controls/bidi; grapheme clipping.
Caps: 4096 rows; 32 spans/row; 1024 bytes/span; 65,536 total spans; 32,768
decoded tables; 1 MiB output/config; 16 MiB/VM. Charge cumulative decode caps
before copying. Bounded regular-file config read, Unix nonblocking open.
Load: 20M instructions/250ms per source. Compose: 50M/500ms per attempt;
hooks every 10k instructions, decode checks every 256 tables. Flash measured
1000 sessions on macOS arm64: release load/compose 0.666/14.10ms; debug
1.375/90.15ms. Observations are not latency guarantees; bounded native work
is not preemptible. Custom+default may
consume two attempt envelopes; exceptional default reload adds a load envelope.
User failure disables custom VM; independent default updates current facts,
then diagnosed native emergency roster if needed. Emergency backoff 10s.
Overflow never silently loses targets; cache faults/rate-limit diagnostics.

Acceptance: default cells/styles/targets at 120/80/60/59/40; custom order/colors/
axis/extent affect paint AND resize; tiny/max/CJK; filter/expand/keys; identity
replacement/collisions; hostile schema/budgets/fallback; offline/input regressions;
both routes, unchanged herd/security. Peer vertical: caret/mouse/drag/history/
resize/capture.
