# 077 — Joining takes one line, quoted or not, and listens where the issuer can reach it

## Before

`remuda cluster invite` printed `remuda cluster join 'FP' 'remuda-join-v1 …'`.
The FP argument is copied from the same output as the line, which already pins
that key, so it checked nothing more. Without a terminal it was mandatory. In
cmd.exe the `'…'` quoting splits the line and the join refused it. The joiner's
listener took the default-route address, so a home laptop on the office VPN
advertised its home Wi-Fi address, and a machine with no private default route
could not join at all. A join rejected for clock skew said only "refused".

## Desired outcome

- invite prints `remuda cluster join 'remuda-join-v1 …'`; the fingerprint is
  shown for the terminal prompt only.
- join accepts the line as one word, as five words, or as cmd.exe passes
  `'…'`; options may follow. `--yes` replaces the prompt when there is no
  terminal. The old `FP 'line'` form still works.
- Without `--bind`, the joiner binds the private address that routes to the
  issuer when it differs from the default-route choice.
- With no private address, the join continues as an outbound-only client.
- 409 from the issuer is reported as clock skew; the line can be reused.

## Expected

Unit tests for word regrouping, `--yes`, route-probe choice with a fake
route table, the fallback decision, and the status mapping. Live tests: a
cmd.exe-style `--yes` join without a terminal, and an outbound-only join under
`REMUDA_TEST_NO_LAN=1` that leaves the saved listener config unchanged.

## Actual

```
$ cargo test -p remuda-native --lib --bin remuda --test cluster_cli_golden \
    --test cli_no_daemon --test cluster_join_units --test cluster_remote \
    --test cluster_revoke_live --test cluster_vfs_live
test result: ok. 1558 passed; 0 failed; 7 ignored   (lib)
test result: ok. 127 passed; 0 failed               (bin)
test result: ok. 34 passed; 0 failed                (cli_no_daemon)
test result: ok. 31 passed; 0 failed                (cluster_cli_golden)
test result: ok. 19 passed; 0 failed                (cluster_join_units)
test result: ok. 11 passed; 0 failed                (cluster_remote)
test result: ok. 15 passed; 0 failed                (cluster_revoke_live)
test result: ok. 7 passed; 0 failed; 5 ignored      (cluster_vfs_live)

# red: edge quotes not stripped
test cluster_cli_tests::join_accepts_the_line_quoted_unquoted_and_as_cmd_exe_passes_it ... FAILED
# red: route source used without the auto-eligible check
test net::advertise_addr::tests::join_listener_keeps_auto_when_routes_agree_or_the_source_is_not_bindable ... FAILED

# two isolated nodes on loopback (token redacted)
$ remuda cluster join remuda-join-v1 127.0.0.1:17491 SHA256:yJI9… F5rO… <TOKEN>
remuda: joining asks for confirmation, but stdin or stderr is not a terminal; add --yes
exit=2
$ remuda cluster join "'remuda-join-v1" 127.0.0.1:17491 SHA256:yJI9… F5rO… "<TOKEN>'" --yes
Joining node-yji99k59 at 127.0.0.1:17491 (fingerprint SHA256:yJI99k59mkyNfBYwugvsDO9p/WYMHIXPF1BIEBC72uE), confirmed by --yes.
Joined node-yji99k59 (fingerprint SHA256:yJI99k59mkyNfBYwugvsDO9p/WYMHIXPF1BIEBC72uE).
Next: remuda cluster remote
```
