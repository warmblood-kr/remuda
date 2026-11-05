# Reading cluster files in a browser (`remuda vfs serve`)

The store (#742) is reachable from a shell (`remuda vfs get`) and from Lua
(`remuda.vfs.read`), but not from a browser. Design documents, reports and
rendered HTML that live at `remuda:///design/…` can only be read one file at a
time, and a page that loads a stylesheet, an image or a second page has no way
to resolve those relative paths. This adds a read-only HTTP bridge on the node
that is looking, so a browser can render a cluster file with its assets.

## Shape

```
remuda vfs serve [--addr 127.0.0.1:PORT] [--root PATH] [--allow-scripts]
```

It prints one line with the URL, then serves until interrupted:

```
Serving remuda:///design/ at http://127.0.0.1:53412/ (read-only; scripts blocked)
```

`--addr` defaults to `127.0.0.1:0`: loopback, an ephemeral port, printed so the
caller can open it. A non-loopback address is refused unless `--allow-remote`
is also given, because everything an admitted node can read would otherwise be
published to the local network.

`--root` is a cluster path prefix (`/design`, default `/`). Requests outside
the prefix are 404, so a viewer can expose one artifact tree and not the whole
store.

## Where it runs

In the CLI process, not the daemon. The server is a *client* of the daemon:
every request becomes one `Request::Vfs` (`get` for content, `ls --json` for a
directory listing). This is the one `remuda vfs` verb that does not run inside
the daemon, for three reasons:

- it blocks for as long as the viewer is open, and the daemon serves one Lua
  image and must not grow a second long-lived listener;
- a port bound by a foreground command is closed by Ctrl-C, with no state left
  in the daemon;
- the store stays owned by exactly one writer, as before.

`remuda vfs serve` still needs a running daemon and never starts one, like the
other verbs.

## Request handling

| Request | Answer |
|---|---|
| `GET /` | the directory listing for `--root` |
| `GET /a/b/` | the listing under `remuda:///a/b/` |
| `GET /a/b.html` | that file |
| `GET /a` when `a` is both a file and a prefix | the file; the listing is at `/a/` |
| `HEAD` | same headers, no body |
| anything else | `405` with `Allow: GET, HEAD` |

Paths are percent-decoded once, then checked by the same rules as `remuda
vfs put` (`remuda_core::vfs`): no `.`/`..` component, no `\`, no NUL or control
characters, 1024 bytes at most. A path that fails those rules is `404`; a
malformed percent escape or a raw control byte in the target is `400`. It is
never normalised and never reaches the store. `..` after decoding is a
traversal attempt, not a directory.

Content type comes from the file extension; unknown extensions are
`application/octet-stream`. `text/*`, `application/json`, `image/svg+xml` and
`application/javascript` are served with `charset=utf-8` only when the
extension says so; the bytes are never rewritten.

An `ETag` is the entry's `sha256`. `If-None-Match` that matches answers `304`
with no body, so a reload after a cluster write is cheap and a viewer can tell
the file changed.

The body comes from a new machine-readable `remuda vfs read`, which always
prints a JSON envelope, not from `get`: one call returns the chosen entry and its bytes, so a `stat` followed by
a `get` cannot race a concurrent write, and a failure arrives as the same code
Lua sees (`not_found`, `too_large`, `no_cluster`, `unreachable`, `corrupt`,
`wrong_cluster`, `bad_uri`, `io`) instead of English on stderr. The bridge
caps a body at 32 MiB and asks `read --max-bytes` for it, so an oversized file
answers `413` from the index without moving the blob. A store error maps to
`404`, `413`, `400`, `502` (corrupt), `503` (unreachable) or `500`.

`ETag` is the entry's sha256, which `read_blob` has already recomputed over the
bytes it returns, so the tag names exactly what the browser got.

## Safety

The bridge renders cluster content in a browser, so the defaults treat a stored
page as untrusted:

- `Content-Security-Policy: sandbox` on every response. On a document — HTML,
  SVG, XML and XHTML, since a browser parses all four — it gives an opaque
  origin, no script, and no way to read another file. The policy names the
  exact bridge origin for `img-src`/`style-src`, and sets `connect-src 'none'`,
  `object-src 'none'`, `base-uri 'none'`, `form-action 'none'` and
  `worker-src 'none'`, so rendered content cannot call the network. Serving a
  policy on non-document types costs nothing and closes the "open it directly
  as an unpoliced document" hole that `application/xml` used to be.
- `--allow-scripts` drops the sandbox for artifacts the operator wrote and
  trusts. The page then runs as the loopback origin: it can read every file
  under the bridge and put what it reads into an outbound navigation.
  `connect-src` never covered navigation, so the docs say that instead of
  claiming no network. Workers stay disabled either way: a worker runs outside
  the document's `connect-src`.
- **Loopback is not private.** Another OS account on the machine can find the
  port and read the served tree with no daemon credentials. `--root` is the
  scope an operator should be willing to show them.
- `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`,
  `Cross-Origin-Resource-Policy: cross-origin` (a sandboxed page must load its
  own CSS and images),
  `Cross-Origin-Opener-Policy: same-origin`.
- No cookies, no `Set-Cookie`, no CORS headers, no request logging of query
  strings.
- `GET`/`HEAD` only; there is no write path at all.
- The listing HTML escapes every name and version it prints.

The store itself is unchanged: any admitted node may already read every file,
so the bridge grants no new authority inside the cluster. What it changes is
exposure: a loopback port, one prefix, and no script by default.

## The HTTP contract

One request per connection, `Connection: close`, never a second request and
never a pipelined byte. The head is capped at 8 KiB and 64 headers; the
request line must be `METHOD SP /path SP HTTP/1.1` (or `1.0`), origin-form
only — absolute-form, authority-form and `*` are refused. A body is refused
outright: any `Transfer-Encoding` or a non-zero `Content-Length` is `400`.
`HTTP/1.1` requires exactly one `Host`, and it must name the address the
bridge bound (or `localhost:PORT` on loopback) to defeat DNS rebinding;
anything else is `421`. Headers are case-insensitive; values with control
characters, and duplicate `Host`/`If-None-Match`, are `400`. Responses always
carry `Content-Length` except `304`, which has no body and no length. `HEAD`
answers exactly the `GET` head with no body. Eight connections are served at
once; a ninth gets `503` and the socket closes.

## Not here

Authentication (the port is loopback and unauthenticated), range requests,
`If-Modified-Since`, directory indexes for every parent of `--root`, and
trimming copies the viewer fetched (a `get` keeps its blob, as documented;
releasing fetched copies is a separate change).

## Tests

Unit tests in `native/src/vfs_serve.rs` cover request parsing, percent-decoding
and traversal refusal, `--root` scoping, extension-to-type mapping, the
conditional `GET`, the HTML listing escape, and every status mapping against a
fake reader.

A live test in `native/tests/cluster_vfs_live.rs` starts a three-node cluster,
puts an HTML page and a stylesheet with one copy each, picks a node that holds
neither, then fetches both through the bridge and checks the bytes, the
sandbox policy, the listing, that `../`, a NUL path and an unknown path are
refused, and that a non-loopback `--addr` is refused without `--allow-remote`.

Still owed, and named here so it is not forgotten: a real-browser check that
the opaque origin blocks script while letting CSS and images render, and that
trusted mode really grants what it says.
