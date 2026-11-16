# 087 — Direct a published VFS document into one ADE

## Before

The HTML TUI can browse manually, but Butler cannot request that a specific
document appear in a user's ADE. Session input identities do not identify ADE
windows, and accepting a request does not prove a document was displayed.

## Desired outcome

For the explicit request “write the design and show it”, publish HTML through
existing VFS put, then directly open its URI in one opted-in local ADE without
typing into its session or losing the user's draft. Report actual presentation
separately from receipt or failure. Prove this path before building routing.

## Expected

No new daemon protocol, listener, dependency or CLI. Start a TUI with
`env REMUDA_DOCUMENT_TARGET=owner remuda cluster remote`. An exclusive private
lock in the local cluster identity directory prevents two live ADEs claiming
this target, including through different daemon sockets.
The opt-in worker writes `remuda:///ade-open/NODE_KEY/owner/ready.json`, containing a
random per-boot `instance`, node fingerprint, base URI, 45-second lease `expires`,
mode and renderer. NODE_KEY is the lowercase SHA-256 hex of the full local node
fingerprint UTF-8 bytes. Replicated VFS entries for another node cannot collide.
It renews every 15 seconds; absence/expired lease is no recipient. No opt-in
means no worker or mailbox activity. Windows opt-in returns Unsupported.

The existing VFS read/put transport handles request.json and result.json under
that same directory. Request fields are instance, positive increasing seq,
uri, expires. URI must be a canonical local remuda:/// document path; mailbox
documents cannot be opened through this action. 4 KiB requests, one pending
request slot, one worker, bounded channels, 3-second IPC deadlines. Admission
requires a future expiry within five minutes. Expiry is an admission deadline;
an accepted read may finish afterward. Send one outstanding request at a time.
Duplicate/older sequence or another boot identity never opens a document.

The action cancels stale/inspection state and navigates the retained Browser,
preserving Back history, session selection and composer draft. It deliberately
opens directly: this opt-in channel is for an explicit user show request,
not unsolicited general notifications. General notifications are not added.
Results are received, rendered, superseded, expired, invalid_uri or load_failed.
Rendered is emitted only after the matching generation loaded successfully,
the document pane has body space, and the frame write/flush succeeded. It
includes actual VFS version/hash and means terminal presentation, not human
attention. A lost result or dead recipient remains unknown, never success.

Publish the document first with `remuda vfs put design.html remuda:///docs/design.html`.
On the SAME node/daemon environment, this uses only existing commands:

```python
import base64, hashlib, json, subprocess, time
node = "REPLACE_WITH_EXACT_NODE_FINGERPRINT_FROM_CLUSTER_NODES"
key = hashlib.sha256(node.encode()).hexdigest()
base = f"remuda:///ade-open/{key}/owner"
def read(name):
    raw = subprocess.check_output([
        "remuda", "vfs", "read", f"{base}/{name}.json",
        "--max-bytes", "4096"])
    envelope = json.loads(raw)
    return json.loads(base64.b64decode(envelope["content"])) if envelope.get("ok") else None
ready = read("ready")
assert ready and ready["node"] == node and ready["expires"] > time.time(), "no live recipient"
request = dict(instance=ready["instance"], seq=time.time_ns(),
               uri="remuda:///docs/design.html", expires=int(time.time())+30)
subprocess.run(["remuda", "vfs", "put", "-",
                f"{base}/request.json"],
               input=json.dumps(request).encode(), check=True)
for _ in range(50):
    result = read("result")
    if result and (result["instance"], result["seq"]) == (request["instance"], request["seq"]):
        if result["status"] != "received":
            print(result)
            assert result["status"] == "rendered", result
            break
    time.sleep(.3)
else:
    raise RuntimeError("presentation unknown; do not claim success")
```

## Actual

```text
direct_document_request_renders_exact_viewer_and_preserves_draft ... ok
test result: ok. 1 passed; 0 failed; finished in 9.99s (initial vertical slice)
final node-namespaced/lock direct-open test: 1 passed; 0 failed; finished in 10.97s
cargo clippy --offline --workspace --all-targets -- -D warnings: passed
cluster_tui integration suite before namespace/lock refinement: 10 passed; 0 failed; finished in 45.19s
cluster_tui::tests unit suite: 192 passed; 0 failed; finished in 3.16s
```

The actual private daemon and two PTYs proved automatic title/paragraph display,
the other ADE unchanged, wrong boot ignored, receipt correlated to URI/hash,
an open while address inspection is active, duplicate after close not reopening,
expired admission, missing-file failure, and returning/submitting the prior draft.
The test publishes fixtures via existing VFS Request, not a fake document fetch.
No real user's screen or shared daemon was changed.

This is a local opt-in prototype: single writer/single outstanding slot; results
are latest-only, not durable delivery history. Shutdown lease may remain for up
to 45 seconds, so even a fresh lease is not proof of delivery. VFS writes/read
visibility use existing cluster trust, not a new permission boundary; instance
is identity, not authorization. Keep control paths on the chosen local node.
No target discovery, remote routing, broadcast, GUI/WebView, UML/image renderer,
notification UI or daemon-restart durability is claimed. A future GUI can share
URI/request/result meaning but must emit its own actual frame completion.
Remote content availability must be checked on the destination; replica count
alone does not identify a viewer or prove that viewer has the blob.
