# Windows session-host endpoint (#902 slice 1)

Transport-only slice of #902: an owner-only Windows named-pipe control
endpoint with capability authentication and bounded framing. It does not make
the Windows session host usable. Unix endpoint/channel code is untouched.

## API (`native/src/session_host/windows.rs`, `cfg(windows)`)

- `Endpoint::bind(private_dir: &Path) -> io::Result<Endpoint>` creates the
  private directory, writes a 32-byte random capability, and reserves the pipe
  name.
- `pub(crate) fn read_capability(private_dir) -> io::Result<[u8; 32]>`.
- `Endpoint::accept(&self, timeout) -> io::Result<Stream>`.
- `pub(crate) fn connect(private_dir, capability: &[u8;32], timeout)
  -> io::Result<Stream>`.
- `read_frame`/`write_frame` reuse `super::wire` with its 64 KiB frame limit.

## Security properties

- Name: `\\.\pipe\remuda-session-host-<32 hex>` from SHA-256 of the canonical
  private directory. Only hex reaches the name; callers cannot add a suffix or
  choose another endpoint.
- Creation: `FILE_FLAG_FIRST_PIPE_INSTANCE` (bind), `PIPE_REJECT_REMOTE_CLIENTS`,
  `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT`, overlapped handles. Two
  instances at most: one connected client plus one reserved listener, so the
  name does not disappear between accepts.
- DACL: exactly the current user, `D:P(A;;GA;;;SID)` built with
  `ConvertStringSecurityDescriptorToSecurityDescriptorW`, protected (no
  inherited ACEs), no handle inheritance, descriptor freed with `LocalFree`.
  The private directory and capability file use the existing
  `windows_security` "secret" helpers: protected DACL for the owner plus LOCAL
  SYSTEM and Administrators (the repository's owner-only Windows policy;
  those two can take ownership anyway). No new dependency.
- Handshake: a fixed 32-byte capability prelude, compared in constant time
  before any framed allocation; then the peer SID via
  `GetNamedPipeClientProcessId` -> `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
  -> `OpenProcessToken(TOKEN_QUERY)` -> `GetTokenInformation(TokenUser)` ->
  `EqualSid` against the current user. Every handle is RAII-closed on all paths.
- Deadlines: every connect, handshake and whole frame has an absolute deadline
  using overlapped I/O plus `WaitForSingleObject`. On timeout or wait failure
  `CancelIoEx` runs, the operation is reaped, and the stream is poisoned
  (`NotConnected`). There is no unbounded read.
- Failure consumption: wrong capability, wrong SID, truncated handshake,
  zero/oversize frame or any I/O error ends that connection; the stream is
  never reused.
- Bounded connection count: `MAX_INSTANCES = 2`; a third concurrent client
  cannot connect until one instance is released.

## Not implemented (later slices)

No ConPTY, child/Job ownership, daemon adoption, host executable changes,
reload/cutover, discovery/inventory, attach UX, or packaging. Nothing here
changes Unix behavior or claims zero-downtime.

## Tests

Unit tests under `#[cfg(all(test, windows))]`: capability + frame round trip;
wrong capability refused on both sides; oversize header refused before payload
and stream poisoned; truncated and silent handshakes refused with bounded
elapsed time; second bind and a second first-instance pipe fail; pipe name
prefix/hex/stability; exact SDDL and constant-time comparison; peer-SID query
fails closed on an unconnected pipe. Tests skip cleanly when the private
endpoint cannot be created. They can only execute on a Windows machine or the
GitHub Windows runner; this branch cross-compiles them only.

## Deferred to physical Windows / later slices

A real second-local-user denial test, native execution of these tests, elevated
daemon versus non-elevated client, domain SIDs, AppContainer/MIC and protected
process behaviour, RDP reconnect, sleep/hibernate, AV/EDR interference,
sustained cancellation and 100-reconnect soak, cross-process endpoint lifetime
and the reload contract. Same-user code remains outside this isolation
boundary, as on Unix.
