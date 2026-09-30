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
  pin = "sha256/BASE64_SHA256_OF_LEAF_SPKI", -- optional leaf certificate pin
  callback = function(result) ... end,       -- required
}

handle:cancel()
```

`request` starts the operation and returns a cancellation handle immediately. Completion is delivered once by invoking `callback(result)` on the daemon's Lua tick, with a result shaped as `{ status, headers, body, error }`. `status` and `headers` are present for an HTTP response; `body` is a Lua string containing response bytes. `error` is absent on success and otherwise contains a stable descriptive string (including timeout, cancellation, TLS, transport, or size-limit failures). TLS errors distinguish certificate expiry, not-yet-valid dates, hostname mismatch, untrusted issuer, and pin mismatch when rustls identifies the reason; platform-specific OS error codes are omitted. For failures before an HTTP response, `status`, `headers`, and `body` are absent. Cancellation is idempotent and the callback fires exactly once: if a result completed before `cancel()` it is delivered as-is; otherwise cancellation delivers a `request cancelled` error. When the calling module is reloaded, an in-flight request remains active and its callback remains registered; cancel it from the module's `stop()` hook if it should not finish after reload.

The callback form is deliberately asynchronous. Network work must run outside the daemon's single Lua thread, and only result delivery may run on the tick. A long-poll held for roughly 30 seconds must leave Lua, timers, and other requests responsive. No `request_sync` form is needed: #213 concerns bounded subprocess execution and Matrix needs no synchronous HTTP path. Do not add a sync form unless a separate core caller demonstrates a need; any future sync operation must have a hard timeout and must not run on the daemon Lua thread.

A validated HTTPS response also includes `peer_certificate = { sha256, spki_sha256, not_before, not_after, trusted }`. `sha256` is the lowercase SHA-256 fingerprint of the leaf certificate DER. `spki_sha256` is the base64-encoded SHA-256 digest of the leaf SubjectPublicKeyInfo DER, without a prefix; use `"sha256/" .. peer_certificate.spki_sha256` as the `pin` value. Validity dates are UTC RFC 3339 timestamps. The field is absent for plain HTTP and only appears after the ordinary TLS verifier accepts the certificate.

For a first connection that needs a TOFU decision, inspect the TLS peer without sending an HTTP request:

```lua
local handle = remuda.http.peer_certificate {
  url = "https://matrix.example/",
  timeout = 10,                              -- required total timeout, seconds
  ca_file = "/path/to/home-ca.pem",          -- optional, same verifier inputs as request
  pin = "sha256/BASE64_SHA256_OF_LEAF_SPKI", -- optional
  callback = function(peer) ... end,         -- required
}

handle:cancel()
```

`peer_certificate` follows the same asynchronous callback and cancellation pattern as `request`. Success calls back with `{ sha256, spki_sha256, not_before, not_after, trusted }`; `spki_sha256` can be prefixed with `sha256/` and passed directly as `pin` to `request`. `reason` is present when `trusted` is false. It runs the configured normal verifier and reports its result while completing only the TLS handshake, including the server handshake-signature check. It sends no HTTP bytes or credentials. SNI and hostname verification use the URL host; non-HTTPS URLs and URLs containing user credentials are rejected. A false trust result never changes the behavior of `request` and does not create a verification bypass.

Lua strings carry method-independent header values and request/response bodies as bytes; the transport does not parse or re-encode JSON. Header names are ASCII case-insensitive HTTP tokens. The returned header map has lowercase names and byte-string values; repeated response fields are joined with `, ` where HTTP permits combination, while `set-cookie` remains a list of values.

## Bounds and behavior

- `timeout` is a hard total wall-clock bound per request, including connect and response transfer. `connect_timeout` is separately bounded and cannot exceed `timeout`.
- `max_bytes` limits response body bytes. Exceeding it aborts the transfer and completes with an error; it is not a truncation limit. Its default is 1 MiB. Callers handling media may raise it to 20 MiB. The request body is also bounded at 20 MiB; this accommodates Matrix media transfers while keeping each request finite. Byte strings are accepted directly; a future streaming API can extend this without changing the request/result words.
- Headers are capped at 64 KiB total per request/response, including names and values. Excess is an error.
- Redirects are disabled (`max_redirects = 0`). A 3xx response is returned as-is; credentials or bodies are never replayed to another origin.
- At most 32 requests may be in flight per daemon. A request over the limit completes asynchronously with an error rather than blocking or queueing without bound.
- The request must use HTTPS for `ca_file` and `pin`. System trust roots are used by default. `ca_file` selects a caller-supplied trust CA for that request (the default system roots are used when it is absent), for self-hosted homeservers. `pin` is the base64-encoded SHA-256 digest of the leaf certificate's SubjectPublicKeyInfo (SPKI), prefixed `sha256/`. When configured, validate the certificate chain and pin during the TLS handshake, before sending any HTTP request bytes. A mismatch fails closed. CA and pin checks may both be configured.
- When the presented server certificate exactly matches a certificate in `ca_file`, it may serve as the leaf certificate even when marked `CA:TRUE`; hostname and validity checks still apply, and TLS handshake signatures are verified by rustls. This narrow fallback bypasses WebPKI path building, so its EKU and unknown critical-extension checks do not run for that exact-match certificate. `rustls-webpki` documents that it does not support using a self-signed end-entity certificate as its own trust anchor.
- Plain `http://` is supported for local and development homeservers; bearer tokens and other credentials sent over it are transmitted in cleartext.
- Never log request headers, response bodies, or request bodies. In particular, `Authorization` must never appear in logs, errors, traces, or diagnostics.

The Matrix client uses this primitive for bearer-authenticated JSON `POST`/`PUT`, ordinary reads, and `/sync` long-polls. The long-poll timeout belongs to this asynchronous request's `timeout`; no tick or daemon thread may wait for it.

## TLS and socket boundary

Use rustls with system roots by default, plus support for a custom CA and the leaf-certificate pin above. The implementation uses rustls 0.23.45 with the ring crypto provider and rustls-platform-verifier 0.7.0. `cargo tree -i rustls` showed no existing rustls dependency before this change. `ca_file` replaces system trust roots for that request, as in curl `--cacert`; it never silently falls back if the file is missing, empty, or invalid. No workspace MSRV is declared. Keep TLS verification and HTTP framing within the network boundary.

The clippy configuration bans `std::net::TcpStream` and `TcpListener` in the policy layer (`core/clippy.toml`). Put all socket operations for this primitive in one `native/src/net/` module, which is also the designated home for cluster PR7's server socket code. Keep every narrowly scoped lint allowance in that module only; document the allowance and this `remuda.http` client alongside the existing server exception in the relevant clippy note/config. Coordinate shared module ownership with `remuda-dev-team-2-lead` before implementation.

## Local tests required before implementation is accepted

Write the tests first (RED) and use only local stub HTTP and HTTPS servers; tests must not access the public network. Cover status, response headers and bytes; timeout; response `max_bytes`; trusted test CA success; leaf pin mismatch with proof no request bytes reached the stub; cancellation; the per-daemon concurrency bound; and daemon/tick responsiveness while a slow request is in flight. Also cover request bodies at the Matrix media size bound, redirect behavior, and that Authorization and bodies are absent from daemon logs. A reusable `remuda_native::net::testing::ScriptedHttpServer` is available to tests that enable the `http-test-support` feature; it serves queued JSON responses by path and keeps an in-memory request record. Use a private daemon for daemon-level tests and stop it and remove its temporary resources afterward.
