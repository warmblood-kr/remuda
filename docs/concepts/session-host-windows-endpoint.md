# Windows session-host endpoint (#902 slice 1)

Transport-only slice of #902: an owner-only Windows named-pipe control
endpoint with capability authentication and bounded framing. It does not make
the Windows host usable. Unix endpoint/channel code is untouched.

## API (`native/src/session_host/windows.rs`, `cfg(windows)`)

- `Endpoint::bind(private_dir)` creates the private directory, writes a 32-byte
  capability and reserves the pipe name.
- `read_capability(private_dir) -> [u8; 32]`; `Endpoint::accept(&self, timeout)
  -> io::Result<Stream>`; `connect(private_dir, capability, timeout) ->
  io::Result<Stream>`.
- `read_frame`/`write_frame` reuse `super::wire` (64 KiB frame limit).

## Security properties

- Name: `\\.\pipe\remuda-session-host-<32 hex>` from SHA-256 of the canonical
  private directory; only hex reaches the name.
- Creation: `FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_REJECT_REMOTE_CLIENTS`,
  `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT`, overlapped handles.
  `MAX_INSTANCES = 16`: one instance is always reserved, so at most 15 clients
  connect at once; a 16th accept cannot create its replacement and latches.
- Reservation: `bind` holds the first instance. `accept` takes it and creates
  the replacement **before** `ConnectNamedPipe`, while the taken instance is
  still unconnected, so one instance holds the name at every moment. An idle
  timeout drops only the timed-out instance; the replacement keeps the name
  and the endpoint usable, as do handshake failure and client close. If the
  replacement cannot be created, the endpoint latches permanently and returns
  `NotConnected`; the taken instance is kept so the name stays held while
  `Endpoint` lives. `take_instance` never falls back to
  `create_instance(name, false)`; with no reserved instance it returns
  `NotConnected`.
- DACL: current user only, `D:P(A;;GA;;;SID)` via
  `ConvertStringSecurityDescriptorToSecurityDescriptorW`, protected, no
  inheritance, freed with `LocalFree`. `pipe_dacl_is_owner_only` reads the
  reserved pipe's kernel DACL with
  `GetSecurityInfo(SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION)` and asserts
  `SE_DACL_PROTECTED`, one access-allowed ACE with no inheritance flags, mask
  `FILE_ALL_ACCESS`, the current-user SID via `EqualSid`, then `LocalFree`. The
  private directory/capability use the existing `windows_security` "secret"
  helpers: owner plus LOCAL SYSTEM and Administrators (repository policy;
  those two can take ownership anyway). This does not claim they are denied.
- Handshake: fixed 32-byte capability compared in constant time before any
  framed allocation; then the peer SID via process id, process token and
  `TokenUser`, ending in `EqualSid`. Handles are RAII-closed.
- Deadlines: every connect, handshake and whole frame has an absolute deadline
  with overlapped I/O; timeout or wait failure runs `CancelIoEx`, reaps the
  operation and poisons the stream. No unbounded read.
- Failure consumption: wrong capability/SID, truncated handshake,
  zero/oversize frame or any I/O error ends the connection; never reused.

## Not implemented (later slices)

No ConPTY, child/Job ownership, daemon adoption, host executable, reload or
cutover, inventory, attach UX or packaging. Unix unchanged; no zero-downtime
claim.

## Tests

Strict Windows-only tests (`#[cfg(all(test, windows))]`); `bound()` panics on
bind error, so there is no skip or early return and a green run proves every
test executed. Each is bounded at two seconds or less. The 12 tests cover
reservation (idle timeout keeps the name; a lost or uncreatable replacement
refuses instead of reacquiring), framing and handshake failures, duplicate
bind refusal, the kernel DACL, name stability, SDDL and constant-time policy,
and peer-SID refusal on an unconnected pipe.

### CI evidence

Main `ci` run
[42166179775](https://github.com/warmblood-kr/remuda/actions/runs/42166179775),
job `test-windows`, GitHub image `windows-2025-vs2026` (`20261103.298.1`),
command `cargo test --workspace --all-targets --no-fail-fast`; the 8 endpoint
tests then existing all passed with zero failed or ignored. The four added
tests (idle timeout, lost reservation, failed replacement, kernel DACL) run in
the next main `ci`. Skips are impossible: `bound()` fails the test.

## What this does not prove

- Cross-user ACL denial: no second local user; the DACL is verified by
  structure, not an actual denied open.
- Remote SMB: `PIPE_REJECT_REMOTE_CLIENTS` is flag-only proof; no remote
  client is exercised.
- Mutual host identity/generation fence: the client checks only the server's
  capability; no generation fence yet.
- Reload/cutover: no endpoint is transferred between hosts.
- Physical devices: RDP reconnect, sleep/hibernate, AV/EDR, elevated daemon
  versus non-elevated client, domain SIDs, AppContainer/MIC, protected process,
  sustained cancellation, 100-reconnect soak, cross-process endpoint lifetime.

Same-user code remains outside this isolation boundary, as on Unix.
