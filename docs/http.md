# `remuda.http` API note

`remuda.http` is a small core transport vocabulary. It is a primitive for sending one HTTP request and delivering one result; caller policy such as Matrix authentication, room allowlists, and rate limits belongs in composites such as `remuda.butler.matrix.request`. This follows the cascading vocabulary in [api-vocabulary.md](api-vocabulary.md): name the small unit first, then build caller-facing composites from it.

## Request and result

```lua
local handle = remuda.http.request {
  method = "POST",                         -- required; ASCII HTTP token
  url = "https://matrix.example/_matrix/...", -- required
  headers = { ["Content-Type"] = "application/json",
              ["Authorization"] = "Bearer " .. token },
  body = json_bytes,                        -- optional Lua string; bytes as-is
  timeout = 35,                             -- required total timeout, seconds
  connect_timeout = 10,                      -- optional; defaults to 10 seconds
  max_bytes = 20 * 1024 * 1024,              -- optional response-body cap
  ca_file = "/path/to/home-ca.pem",          -- optional custom trust CA
  pin = "sha256/BASE64_SHA256_OF_LEAF_CERT", -- optional leaf certificate pin
  callback = function(result) ... end,       -- required
}

handle:cancel()
```

`request` starts the operation and returns a cancellation handle immediately. Completion is delivered once by invoking `callback(result)` on the daemon's Lua tick, with a result shaped as `{ status, headers, body, error }`. `status` and `headers` are present for an HTTP response; `body` is a Lua string containing response bytes. `error` is absent on success and otherwise contains a stable descriptive string (including timeout, cancellation, TLS, transport, or size-limit failures). For failures before an HTTP response, `status`, `headers`, and `body` are absent. Cancellation is idempotent; if completion has already been delivered it has no effect, and a cancelled in-flight request reports `error` once through the callback.

The callback form is deliberately asynchronous. Network work must run outside the daemon's single Lua thread, and only result delivery may run on the tick. A long-poll held for roughly 30 seconds must leave Lua, timers, and other requests responsive. No `request_sync` form is needed: #213 concerns bounded subprocess execution and Matrix needs no synchronous HTTP path. Do not add a sync form unless a separate core caller demonstrates a need; any future sync operation must have a hard timeout and must not run on the daemon Lua thread.

Lua strings carry method-independent header values and request/response bodies as bytes; the transport does not parse or re-encode JSON. Header names are ASCII case-insensitive HTTP tokens. The returned header map has lowercase names and byte-string values; repeated response fields are joined with `, ` where HTTP permits combination, while `set-cookie` remains a list of values.

## Bounds and behavior

- `timeout` is a hard total wall-clock bound per request, including connect and response transfer. `connect_timeout` is separately bounded and cannot exceed `timeout`.
- `max_bytes` limits response body bytes. Exceeding it aborts the transfer and completes with an error; it is not a truncation limit. Its default is 1 MiB. Callers handling media may raise it to 20 MiB. The request body is also bounded at 20 MiB; this accommodates Matrix media transfers while keeping each request finite. Byte strings are accepted directly; a future streaming API can extend this without changing the request/result words.
- Headers are capped at 64 KiB total per request/response, including names and values. Excess is an error.
- Redirects are disabled (`max_redirects = 0`). A 3xx response is returned as-is; credentials or bodies are never replayed to another origin.
- At most 32 requests may be in flight per daemon. A request over the limit completes asynchronously with an error rather than blocking or queueing without bound.
- The request must use HTTPS for `ca_file` and `pin`. System trust roots are used by default. `ca_file` adds a caller-supplied CA for self-hosted homeservers. `pin` is the base64-encoded SHA-256 digest of the leaf certificate's DER bytes, prefixed `sha256/`. When configured, validate the certificate chain and pin during the TLS handshake, before sending any HTTP request bytes. A mismatch fails closed. CA and pin checks may both be configured.
- Never log request headers, response bodies, or request bodies. In particular, `Authorization` must never appear in logs, errors, traces, or diagnostics.

The Matrix client uses this primitive for bearer-authenticated JSON `POST`/`PUT`, ordinary reads, and `/sync` long-polls. The long-poll timeout belongs to this asynchronous request's `timeout`; no tick or daemon thread may wait for it.

## TLS and socket boundary

Use rustls with system roots by default, plus support for a custom CA and the leaf-certificate pin above. `cargo tree -i rustls` on this branch finds no rustls dependency (only `mio` is present in `Cargo.lock`), so the implementation will need to add the TLS crates and should choose versions compatible with the workspace MSRV and lockfile. Keep TLS verification and HTTP framing within the network boundary.

The clippy configuration bans `std::net::TcpStream` and `TcpListener` in the policy layer (`core/clippy.toml`). Put all socket operations for this primitive in one `native/src/net/` module, which is also the designated home for cluster PR7's server socket code. Keep every narrowly scoped lint allowance in that module only; document the allowance and this `remuda.http` client alongside the existing server exception in the relevant clippy note/config. Coordinate shared module ownership with `remuda-dev-team-2-lead` before implementation.

## Local tests required before implementation is accepted

Write the tests first (RED) and use only local stub HTTP and HTTPS servers; tests must not access the public network. Cover status, response headers and bytes; timeout; response `max_bytes`; trusted test CA success; leaf pin mismatch with proof no request bytes reached the stub; cancellation; the per-daemon concurrency bound; and daemon/tick responsiveness while a slow request is in flight. Also cover request bodies at the Matrix media size bound, redirect behavior, and that Authorization and bodies are absent from logs. Use a private daemon for daemon-level tests and stop it and remove its temporary resources afterward.
