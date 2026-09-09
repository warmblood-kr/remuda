# 005 — Asking the session instead of assuming it

## Before

Step 004 gave Lua an input vocabulary: `insert`, `key`, `click`. It works, and
its tests are green. They are green inside a world that does not exist.

Three values are hardcoded in `core/src/keys.rs` as if they were facts about
terminals. Each is really a piece of *state held by the program on the other end
of the pty*:

```
what we emit                    what actually decides it
─────────────────────────────── ────────────────────────────────────────────
key("up") → ESC [ A             DECCKM. A program that sent ESC[?1h expects
                                ESC O A, and readline, vim and ncurses apps
                                all set it.
click(...) → SGR mouse report   The program's mouse mode. If it never enabled
                                reporting, the bytes are ignored — a click
                                that "succeeds" and does nothing.
one cell = one character        Wide characters. 한글 and 漢字 occupy two
                                columns, so a column index computed by
                                counting characters in `capture()` output is
                                wrong on any line containing them.
```

정수님, 2026-09-10: *"cjk 등에 대한 처리도 염두에 둬야합니다. putty 등, 여러
터미널 에뮬레이터들의 경험과 지혜를 참고합시다."* and then, widening it:
*"꼭 cjk 말고도. term 환경변수에 따라 달라지는 것들이라던지. 이맥스가 버퍼에
대해서 제공하는, describe char 인가 하는, 조사(inspect)를 위한 함수들도 필요할거고."*

Those are not three defects. They are one: **we encoded constants where the
session holds variables.** And the third one is the tell that this is a missing
*capability*, not three missing branches — there is no way for a script to ask
what is in a cell at all. `capture()` returns a flat string; everything past
that is the caller guessing.

The wide-character case is the one that already bites a shipped feature: a
script that finds a word in `capture()` and clicks it lands in the wrong place
the moment the line has Korean in it. Step 004 called that layer finished.

**What the ambiguity is NOT.** Two things were checked and are fine, so this
step does not touch them:

- `keys::key()` on a CJK character. `char::encode_utf8` into `[0u8; 4]` is
  correct for every Unicode scalar (4 bytes is the maximum), and the suite
  already asserts `bytes("가")`. Display width never enters `key()` — it deals
  in one scalar's UTF-8 length, which is a different quantity.
- `script.rs`'s `insert`. Lua strings are byte strings and we forward bytes
  unaltered, so CJK text passes through transparently. There is nothing to
  encode and therefore nothing to get wrong.

**And IME does not apply here.** Composition — candidate windows, provisional
glyphs — happens in a keyboard client before any byte reaches a pty. `insert`
receives finished UTF-8; `key` models one physical keypress. Neither is a
composition pipeline, so PuTTY's IME wisdom is about a layer remuda does not
occupy.

## Desired outcome

The three constants become questions, and Lua gains the ability to ask one
directly.

The decisive finding is that **nothing needs to be computed** — every value is
already parsed and already public. `vt100::Screen` has been tracking the
program's mode-setting escapes since step 001, and `vt100::Cell` recorded each
cell's committed width when it laid the screen out. We have not been unable to
know; we have not been asking.

That matters beyond convenience. Had the width been unreadable, the tempting
move would be to compute it ourselves with `unicode-width` — and then there are
**two width oracles that can disagree**, so on the day they do, `capture()` and
`click()` point at different places and neither is wrong on its own terms. The
cell's own recorded width is correct *by construction*: it is the number the
screen model used to decide where the next cell goes.

So:

1. `Session::input_mode()` reports what the program has enabled — application
   cursor keys, application keypad, bracketed paste, mouse mode and encoding.
2. `keys::key(spec, mode)` takes that mode. It stays a pure function in the
   policy layer, taking a small plain struct — no pty, no session, still
   testable by table. The arrows and F-keys emit SS3 or CSI according to
   DECCKM instead of always CSI.
3. `click` refuses when the program has mouse reporting off, rather than
   emitting bytes into a program that will discard them. This is the same
   empty-success refusal `key()` already makes for an unknown key name.
4. A new `remuda.describe(name, col, row)` returns what is in one cell: its
   contents, whether it is wide, whether it is the *continuation* half of a
   wide character, and its colours and attributes. This is the screen-cell
   analogue of Emacs's `describe-char`, and it is what makes a column index
   trustworthy: a script can now find where a wide character ends rather than
   assuming.

**What is deliberately not built.**

- Streaming. It was announced as step 005 and is displaced to 006; this is
  the more load-bearing half and it is a prerequisite for a script that wants
  to react to what it sees.
- Wrapping `insert` in bracketed-paste markers when the program has enabled
  it. `input_mode()` will report the flag, so a Lua script can do it. Whether
  the *primitive* should do it silently is a policy question, and a primitive
  that sometimes adds bytes is not the atom this repo has been careful to
  keep.
- Configuring ambiguous-width. `vt100` takes `unicode-width`'s default
  (Ambiguous → narrow) and exposes no `cjkWidth`-style knob. That is PuTTY's
  and xterm's real setting and a real residual risk — if the *remote program*
  disagrees with `unicode-width` about a box-drawing glyph, our screen model
  diverges from what it drew. It is out of reach from here and not worth a
  fork; it is recorded as a known limit, not fixed.
- Reading terminfo. tmux does both halves asymmetrically — it *pins* TERM for
  its children so its own emulator has one terminal to implement, and reads
  terminfo for the outer terminal it draws to. remuda has no outer terminal; it
  is the endpoint. So only the pin half applies, and a database lookup would
  answer a question we get to set the answer to.

**TERM, however, is worse than "we decide it" and is folded into this step.**
We do not decide it. We never set it, and `portable-pty` inherits the daemon's
whole environment, so the child's TERM is whatever launched the daemon — Emacs,
systemd, a login shell, or nothing:

```
$ grep -rn 'TERM' --exclude-dir=target --exclude-dir=.git .
.github/workflows/ci.yml:16:  CARGO_TERM_COLOR     ← unrelated; 1 hit total
$ grep -rn 'SHELL' ... | wc -l
3                                                   ← positive control
native/src/daemon.rs:121-134   builder sets argv and cwd. Nothing else.
portable-pty-0.9.0/src/cmdbuilder.rs:74-86   get_base_env() = std::env::vars_os()
```

That is the same defect as the other three wearing different clothes: a value
that decides what our bytes mean, left to whatever happened to be in the
environment. Pinning it costs one line and makes the hardcoded xterm sequences
correct *by construction* rather than by luck. DECCKM stays a runtime question
regardless — a program flips it mid-session, so no TERM value can answer it.

**And the frozen API is the test of whether this is compatible.** `key`'s Lua
signature does not change — the binding reads the mode itself — so
`native/tests/api/v1.lua` must still run untouched. Principle 10 gets its first
real exercise on a change that was not designed to trip it.

## Expected

1. `Session::input_mode()` returns a struct whose fields track the program's
   escapes: sending `ESC[?1h` through a session flips `application_cursor` to
   true, and `ESC[?1l` flips it back.
2. With `application_cursor` set, `key("up")` produces `ESC O A`; with it
   clear, `ESC [ A`. F1–F4 are unaffected (already SS3); modified keys stay
   CSI in both modes, since SS3 has no parameter slot.
3. `click` on a session whose program has not enabled mouse reporting returns
   an error naming that, and writes nothing to the pty.
4. `describe(name, col, row)` on a cell holding `가` reports width 2 and
   `continuation = false`; the cell one column to its right reports
   `continuation = true` with empty contents.
5. `native/tests/api/v1.lua` runs unchanged and green.
6. Each new guard is shown failing under a planted defect, and failing for the
   stated reason rather than merely failing (principles 2 and 3).

## Actual

Capability probe first, since the whole design turns on it. Read from the
vendored crate source, not from the documentation and not from memory:

```
$ V=$(echo ~/.cargo/registry/src/index.crates.io-*/vt100-0.16.2)
$ grep -n "pub fn" $V/src/cell.rs
89:    pub fn contents(&self) -> &str {
95:    pub fn has_contents(&self) -> bool {
101:    pub fn is_wide(&self) -> bool {
109:    pub fn is_wide_continuation(&self) -> bool {
135:    pub fn fgcolor(&self) -> crate::Color {
141:    pub fn bgcolor(&self) -> crate::Color {
148:    pub fn bold(&self) -> bool {
155:    pub fn dim(&self) -> bool {
162:    pub fn italic(&self) -> bool {
169:    pub fn underline(&self) -> bool {
176:    pub fn inverse(&self) -> bool {

$ grep -nE "pub fn" $V/src/screen.rs | grep -E "cursor|mouse|paste|cell|keypad"
489:    pub fn cursor_position(&self) -> (u16, u16) {
512:    pub fn cursor_state_formatted(&self) -> Vec<u8> {
534:    pub fn cell(&self, row: u16, col: u16) -> Option<&crate::Cell> {
554:    pub fn application_keypad(&self) -> bool {
560:    pub fn application_cursor(&self) -> bool {
566:    pub fn hide_cursor(&self) -> bool {
572:    pub fn bracketed_paste(&self) -> bool {
578:    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
584:    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
```

Every one of the three assumptions has a getter sitting opposite it, and the
`Cell` accessor list is very nearly the field list `describe-char` prints. The
width is recorded where it was decided:

```
$ grep -nE "unicode_width|set_wide" $V/src/cell.rs
1:use unicode_width::UnicodeWidthChar as _;
51:        self.set_wide(c.width().unwrap_or(1) > 1);
113:    fn set_wide(&mut self, wide: bool) {
121:    pub(crate) fn set_wide_continuation(&mut self, wide: bool) {
```

And that this repo has never touched any of it — the searched-and-absent side,
with the transitive hit as the positive control proving the search ran:

```
$ grep -rnE "unicode_width|wcwidth|is_wide|double_width|CJK" --include='*.rs' core/ native/
$ echo "exit=$?"
exit=1

$ grep -n "unicode-width" Cargo.lock
443:name = "unicode-width"
455: "unicode-width",          ← vt100 0.16.2's dependency list
```

The API is available and has never been called once:

```
$ grep -rn "\.cell(" native/src core/src | wc -l
0
```

And `vt100` has no ambiguous-width knob, which is what puts PuTTY's checkbox
out of reach from here:

```
$ grep -rn "ambiguous|CJK" $V/src/ | wc -l
0
$ sed -n '51p' $V/src/cell.rs
        self.set_wide(c.width().unwrap_or(1) > 1);   ← unicode-width's default
```

One more thing worth recording, from the Emacs manual (`emacs.info:16556-16575`,
on what `describe-char` displays). The last bullet:

> • If you are running Emacs on a graphical display, the font name and glyph
> code for the character. **If you are running Emacs on a text terminal, the
> code(s) sent to the terminal.**

Emacs already counts *"what bytes would this be on a terminal"* as an
inspection field. That is `keys::key()` read backwards — which is a decent sign
that inspection and input encoding belong to one another, and that having built
only the second half was the actual omission.

Implementation results follow.
