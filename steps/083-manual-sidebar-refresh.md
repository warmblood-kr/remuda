# Manual session sidebar refresh (#939)

## Before

Session list keys already trigger ordinary refreshes, but there is no named
manual refresh control. An unchanged frame is not repainted. Compact list
packing is already fixed on main; this change must protect that behavior.

## Desired outcome

Default `r` in session list browse re-reads the list and installed Lua
decorators, then repaints even an unchanged frame. It sends no session input.
Rebinding, unbinding and help use the existing Lua registry. Other input modes
and dead-daemon restart retain their behavior.

## Expected

- A permanent key-registry test fails before the new action exists, then passes.
- Pure input tests cover browse, raw focus, composer, prompt, confirm and visual
  mode, plus remapping, removal and help.
- A private in-process daemon reads changed file-backed model evidence through
  real List/Eval requests; two sessions grow to three without losing selection
  or ordered rows. Missing model evidence stays `?`; a failed Eval remains visible.
- Pure renderer checks cover dense compact and stacked rows, layout/order
  headers, selection and one rendered occurrence of every member at two and
  three sessions.

## Actual

Captured before implementation:

```text
$ cargo test --offline -p remuda-native --lib session_refresh_default_rebind_unbind_and_help_share_the_registry -- --nocapture
running 1 test
assertion `left == right` failed
  left: None
 right: Some("session.refresh")
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1864 filtered out; finished in 0.03s
```

Captured after implementation (private socket test outside the restricted
socket sandbox; local host built and copied beside the test binary using the
existing hosted-suite convention):

```text
$ cargo test --offline -p remuda-native --lib session_refresh_default_rebind_unbind_and_help_share_the_registry -- --nocapture
running 1 test
test keybindings::tests::session_refresh_default_rebind_unbind_and_help_share_the_registry ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1868 filtered out; finished in 0.03s

$ cargo test --offline -p remuda-native --lib manual_refresh -- --nocapture
running 3 tests
test tui::composer_tests::manual_refresh_is_confined_to_browse_and_preserves_other_r_input ... ok
test tui::composer_tests::manual_refresh_rebind_and_unbind_change_dispatch_and_footer ... ok
test tui::tui_unit::manual_refresh_rereads_file_backed_model_and_herd_through_private_daemon ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 1866 filtered out; finished in 1.25s

$ cargo test --offline -p remuda-native --lib local_list_growth_preserves_dense_order_selection_and_layout_header -- --nocapture
running 1 test
test image::tests::local_list_growth_preserves_dense_order_selection_and_layout_header ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1868 filtered out; finished in 0.01s

$ cargo test --offline -p remuda-native --lib manual_refresh_rereads_file_backed_model_and_herd_through_private_daemon -- --nocapture
running 1 test
test tui::tui_unit::manual_refresh_rereads_file_backed_model_and_herd_through_private_daemon ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1868 filtered out; finished in 1.61s
```

## Limits

Model labels depend on installed decorator evidence. Butler's Codex adapter
can fall back to its launch model or `?` when current status is absent. This
does not change a model, reload Lua configuration or invalidate Butler's
activity cache. Remote session screens repaint the latest received snapshot;
no forced remote network requery or cluster-picker change is included.
