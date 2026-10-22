# 061 — fence the writer epoch at the session host

## Before

A daemon's right to write is its inventory writer fence (store incarnation,
epoch, nonce), raised by every `Owner::claim`. The host never saw it.
`HostSession::controller()` asked the store `owner.current()` and then sent
`Acquire` as a separate step. A daemon that passed the check just before a
newer daemon claimed could still take the host lease and resize, close or stop.
The host's own controller epoch is a lease counter, not the durable writer, so
it could not tell the two daemons apart. Input acts had the same gap between
the `Adopt` commit and `Acquire` in `Owner::resume`.

Nothing tested a daemon and a host from different builds against each other,
which is the normal state during `remuda reload`.

## Desired outcome

- A host that advertises `writer-fence-v1` takes `AcquireWriter { writer }`,
  where `writer` is the daemon's (store, epoch). It keeps the highest writer it
  has seen and refuses an older epoch or another store with
  `superseded_writer`. The high-water rises before the lease check, so an older
  holder that still has the lease gets `superseded_writer` on its next input,
  resize, close or stop. It can still release.
- The change is additive. The new request is a new variant. A daemon sends it
  only when the host advertises the capability, and otherwise sends the plain
  `Acquire` as before. A plain `Acquire` (from an older daemon) is still
  granted, so a downgrade keeps working.
- CI job `host-compat-n-plus-one` builds the base commit (N) and runs a hosted
  session handover both ways: N creates and N+1 adopts, then N+1 creates and N
  adopts. Each adopter sends input and closes.

## Expected

- `superseded_writer` for every effect of an older writer, at the host.
- `{"op":"Acquire"}` is unchanged on the wire.
- Measured while building: serde's `deny_unknown_fields` does not reject an
  extra field on a unit variant of the internally tagged `Request`. An
  `Acquire` with an added `writer` field would be silently accepted by an older
  host, which would then look fenced while it is not. Struct variants and
  unknown variants are rejected, and `Response` ignores unknown fields.
- Both N/N+1 directions pass, and each goes red when its planted break is in.

## Actual

```
$ cargo test -p remuda-native --lib -- session_host::control session_host::server::writer_fence
test session_host::control::tests::a_newer_writer_fences_an_older_holder_and_older_writers ... ok
test session_host::server::writer_fence_tests::older_peers_frames_are_unchanged_and_still_accepted ... ok
test session_host::server::writer_fence_tests::the_host_refuses_resize_close_and_stop_from_a_superseded_writer ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 1391 filtered out; finished in 0.13s

$ cargo test -p remuda-native --lib -- session_host::remote::tests::the_host
test session_host::remote::tests::the_host_refuses_an_older_writer_that_already_passed_the_store_check ... ok

serde probe (removed after use):
{"op":"Acquire","writer":1}                      => Ok
{"op":"Resize","rows":30,"cols":100,"writer":1}  => Err("unknown field `writer`, expected `rows` or `cols`")
{"op":"Bogus"}                                   => Err("unknown variant `Bogus`, …")
Response {"result":"Granted","epoch":1,"extra":2} => Ok

$ REMUDA_COMPAT_PREVIOUS=<main 12e0f51 build> cargo test -p remuda-native --test session_host_daemon -- --ignored n_plus_one
test n_plus_one_older_daemon_adopts_a_newer_host_and_inventory ... ok
test n_plus_one_daemon_adopts_an_older_host_and_inventory ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 4.24s

planted: Client::acquire sends AcquireWriter without checking the capability
test n_plus_one_older_daemon_adopts_a_newer_host_and_inventory ... ok
test n_plus_one_daemon_adopts_an_older_host_and_inventory ... FAILED
  left: Error("agent io error: input outcome unknown; recover the original request before retry")
 right: Ok

planted: the host refuses a plain Acquire once it has seen a fenced writer
test n_plus_one_daemon_adopts_an_older_host_and_inventory ... ok
test n_plus_one_older_daemon_adopts_a_newer_host_and_inventory ... FAILED
  left: Error("agent io error: input outcome unknown; recover the original request before retry")
 right: Ok
```

## Limits

- The host knows about a newer writer only after that writer reaches it. Until
  the new daemon touches a host, for example while adopting it, an older daemon
  that passed its store check can still act there once. A reload should touch
  every host before it treats the old daemon as fenced.
- A daemon from before this change sends only a plain `Acquire`, so the host
  cannot fence it. Only the store check guards it, as before.
- The planted breaks were run by hand, not in `gates-can-fail`.
