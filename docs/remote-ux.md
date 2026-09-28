# Remote session UX draft

This is a UX proposal for `remuda remote HOST/SESSION`. It covers one session at a time over short request/response exchanges, so a lost network request does not end the viewing command. The node name and session remain visible throughout. Nodes are identified by their cluster name; the later composed multi-node view comes separately. The name `control tower` is reserved for that later view.

All commands below are proposed user-facing syntax for review. The cluster and remote subcommands and their flags are not implemented yet. Example addresses, fingerprints, tokens, and output are illustrative.

## Journey 0: form a cluster

On the first machine, initialize a node. The command creates its node key pair and starts the cluster endpoint on the VPN address. It prints the address, public-key fingerprint, and a short-lived, single-use token in one pasteable line:

```text
$ remuda cluster
Cluster initialized
Node: studio
Address: 100.80.0.12:7443
Fingerprint: SHA256:QmFzZTY0LWZpbmdlcnByaW50LWV4YW1wbGU=
Join command (expires in 10 minutes; single use):
curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | REMUDA_CHANNEL=nightly sh && "$HOME/.local/bin/remuda" cluster join --address 100.80.0.12:7443 --fingerprint SHA256:QmFzZTY0LWZpbmdlcnByaW50LWV4YW1wbGU= --token 'eyJleGFtcGxlLW9uZS10aW1lLXRva2Vu'
```

The operator shares that line with the intended machine over a trusted channel. The token is a bearer secret: it is shown once, expires quickly, and is consumed once. The joining machine pastes it into a shell; the first node's fingerprint is pinned before the secure channel is established, then the joiner registers its own public key. No public discovery or NAT traversal is implied; both nodes are expected to reach one another on the same VPN.

```text
$ curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | REMUDA_CHANNEL=nightly sh && "$HOME/.local/bin/remuda" cluster join --address 100.80.0.12:7443 --fingerprint SHA256:QmFzZTY0LWZpbmdlcnByaW50LWV4YW1wbGU= --token 'eyJleGFtcGxlLW9uZS10aW1lLXRva2Vu'
Downloading remuda…
Pinned cluster node fingerprint verified
Joining cluster…
Joined cluster as node: field-laptop
```

Operators can inspect membership and revoke a node key:

```text
$ remuda cluster nodes
NODE          ADDRESS          STATUS
studio        100.80.0.12:7443 online
field-laptop  100.80.0.27:7443 online

$ remuda cluster revoke field-laptop
Revoke node field-laptop (SHA256:Vmlld2VyLWtleS1leGFtcGxl)? [y/N] y
Node field-laptop revoked. Existing connections closed.
```

The specific list formatting and confirmation syntax are proposed UX. These example commands show the POSIX installer; a PowerShell host should print an equivalent install-and-join line for that shell. A token or fingerprint error is fatal and must not fall back to an unpinned connection; unreachable nodes can be retried after the VPN is repaired:

```text
Join failed: token expired or already used. Run `remuda cluster` on the first node to create a new join command.
Join failed: fingerprint mismatch for 100.80.0.12:7443. Expected SHA256:QmFz...; received SHA256:YW5vdGhlci1rZXk. Join aborted; verify the command with the cluster operator.
Join failed: 100.80.0.12:7443 is unreachable. Check that both machines are on the VPN, then retry the join command.
```

## User journeys

### 1. Host a session

On the machine that runs the session, start it with its normal command and a stable session name:

```sh
remuda run -n dev claude
```

The daemon owns the session after the initiating terminal detaches or disappears. The host operator gives a registered node name and session name to the viewer. The session is not exposed publicly: remote access is through the VPN, and the selected host must authorize the viewer. Starting a session and granting remote-view access are separate actions.

A viewer connects from a second machine with:

```sh
remuda remote laptop/dev
```

The initial command has no flags. Proposed optional flags are `--read-only` to explicitly request viewing without input rights and `--poll INTERVAL` to adjust the refresh interval (default: 3s). They are proposals for review, not implemented CLI behavior.

### 2. Connect from another machine

The node name `laptop` resolves through authenticated cluster membership. `dev` selects the session on that host. While connecting, the command names both pieces so the user can spot a mistaken target:

```text
remuda remote laptop/dev
laptop / dev · connecting…
Resolving host and authorizing session
```

After authorization, the client shows the most recent captured screen and its age. The selected session is a snapshot view, not a promise of per-keystroke interactive-terminal timing. Entered lines are delivered as input to the session.

### 3. Network drop and resume

During a VPN or Wi-Fi interruption, keep the last known snapshot on screen and continue to display its age. New input can still be composed locally but is visibly pending. The client retries short sync and input requests in the background. When the host returns, it resumes from the last seen screen version and retries pending input using the same batch ID.

```text
laptop / dev · stale · reconnecting (last snapshot 28s ago)
─────────────────────────────────────────────────────────
$ Please check the migration output

Draft · 37 chars · waiting to send
Network unavailable. Retrying…
```

A line remains in the local draft until the send is acknowledged. Once submitted, it appears in a pending queue until acknowledged. A timeout is ambiguous, so the client must retry that exact batch ID and bytes; it must not create a new batch for a retry. Show `sent` only after the daemon acknowledges the batch. If the daemon cannot establish whether an old batch was applied (for example, its deduplication record has expired), stop and report an uncertain delivery instead of silently resending under a new ID.

### 4. Two viewers

Two clients may open the same session concurrently. Both can view snapshots and see their own snapshot age and connection state. Input rights should be exclusive per session: recommend that the first authorized writer holds a renewable writer lease and a second viewer opens read-only. The second viewer sees `read-only · another viewer is sending input` and may request the lease if the first leaves or explicitly releases it. A race to acquire a lease is resolved by the host; never merge simultaneous independent line editors into one input stream.

A viewer may also select `--read-only` explicitly. That viewer can never accidentally type into the session, even when no other writer is connected. If a writer disconnects, keep its lease briefly while it reconnects so its queued batches can be reconciled; after lease expiry, another viewer may acquire write access. Pending input must be acknowledged or visibly reported as uncertain before a lease is transferred.

### 5. Switch sessions or nodes

Press Esc to detach from the current view, then run `remuda remote laptop/worker` or `remuda remote buildbox/dev`. These always start a fresh target selection and display the new `HOST / SESSION` before accepting input. If the current draft is nonempty, ask whether to keep it locally or discard it; pending batches stay associated with their original host/session and are never retargeted. Joined node names resolve through cluster membership; this command still opens one session at a time and does not show a multi-node overview.

### 6. End the view or end the session

Press Esc to leave the remote view. If a local draft has not been sent, ask whether to keep it in the local client for later or discard it. Detaching does not stop the remote session. Reopen it with `remuda remote laptop/dev`.

Ending the session is a separate, destructive action. Recommend an explicit confirmation in the remote view (`Ctrl-C` twice, with a confirmation prompt) or an explicit close command; do not map Esc or ordinary input to session termination. On the host, the existing Lua close API can end the session:

```sh
remuda -e 'remuda.close("dev")'
```

When the remote session exits or is closed, stop retrying input and show its final captured screen with an ended marker. Any unsent draft stays local and must not be transmitted to a newly created session that reuses the same name.

## Screen states

The status line always includes `HOST / SESSION`. While the client is connected, the snapshot age reflects when that screen was captured, not when it was rendered locally.

### Connecting

```text
laptop / dev · connecting…
Resolving host and authorizing session
```

### Live and writable

```text
laptop / dev · live · snapshot 1s ago · writer
───────────────────────────────────────────────
$ Waiting for test results…

> Type a line; Enter sends · Esc leaves
```

### Stale and reconnecting

```text
laptop / dev · stale · reconnecting · snapshot 28s ago
──────────────────────────────────────────────────────
$ Waiting for test results…

Draft · 37 chars · waiting to send
Network unavailable. Retrying…
```

### Read-only

```text
laptop / dev · live · snapshot 1s ago · read-only
──────────────────────────────────────────────────
$ Waiting for test results…

Another viewer has the writer lease. Press r to request it; Esc leaves.
```

### Ended

```text
laptop / dev · ended · final snapshot 2s ago
────────────────────────────────────────────
Process exited (status 0). Input is disabled.
Esc leaves.
```

Other actionable failures should name the target and next step: `laptop / dev · access denied — ask the host operator for access`; `laptop / dev · session not found — check the session name`; `laptop · host unreachable — check VPN, retrying`; `laptop / dev · delivery uncertain — batch <id> may have been applied; do not resend as a new line`.

## Input composition and special keys

Recommend a local line editor instead of forwarding every key. Text, cursor movement, Backspace, Delete, and Home/End edit the draft on the client. Enter commits one line to the pending queue and sends it as one idempotent batch. Show the pending line (or a short preview) with `waiting to send`, `sending`, and `sent` acknowledgement states. Editing after Enter creates a new draft and never mutates an already submitted batch.

While disconnected, keystrokes are accumulated in the current local draft; pressing Enter freezes that line into one pending batch. Pending batches preserve order. A retry after a timeout reuses the original batch ID and bytes. Show each pending line distinctly so users can tell what has not yet been acknowledged. For a read-only viewer, input is disabled and no characters are queued.

Do not silently reinterpret terminal control keys. Recommend: arrows edit the local line; Ctrl-C cancels the current local draft; Esc leaves the view; an explicit `Ctrl-C` confirmation action signals an interrupt to the remote process. Other control keys such as Esc-as-application-input, Tab, or function keys should require a named `send-key` action and confirmation of the target until the interaction model is reviewed. This first UX is for sending lines, not a full-screen remote terminal; applications that require arbitrary control sequences are out of scope for this screen.

## Open UX decisions

1. **Input:** send each key immediately, flush buffered chunks on a timer, or edit a line and send on Enter. Recommend the line editor: it works with mobile keyboards, allows correction before send, and makes an exactly-once unit visible. Its trade-off is that it is not suitable for full-screen applications.
2. **Two viewers:** allow all viewers to write with interleaved input, make every extra viewer read-only, or grant one renewable writer lease. Recommend the lease: it gives shared read access without surprising input interleaving. The lease duration and handoff policy need owner review.
3. **Leaving vs ending:** overload a terminal key, provide an explicit close action, or separate detach and confirmed termination. Recommend separate detach and confirmed termination; network loss or Esc must never kill the host session.
4. **Host configuration:** accept arbitrary VPN URLs on each invocation or use named configured hosts. Recommend registered node names (`NODE/SESSION`) so a target is short, stable, and easy to verify; cluster membership is the source of truth.

## Protocol implications

Sync returns a snapshot or diff with a monotonically increasing version; reconnect resumes from the last version. A committed line is `input(session, batch_id, bytes)`, and retries reuse the same ID and bytes so an acknowledged or previously applied batch is never delivered twice. Local requests can reuse the daemon socket. The remote channel uses Remuda’s own short-lived authenticated request/response protocol over the VPN, with joined node keys. Use off-the-shelf channel crypto: the current direction is Noise IK via `snow`, with pinned keys. The protocol needs multiple readers and a host-enforced writer lease. Per-session output events from #190 may later wake a long-poll without changing these user journeys.

### Security and implementation constraints

- The allowlisted request front comes first, initially on a local socket. It exposes only the operations required by this UX; never forward the general daemon protocol or arbitrary Lua remotely. The network listener is a later layer over that restricted front.
- If the listener needs TCP, allow exactly one scoped clippy TCP-ban exception in its module (`#[allow]` at that module), with a `clippy.toml` and documentation note that names #191. Bind only to the configured VPN address; never default to `0.0.0.0`. Start the listener only after `remuda cluster` initialization.
- Join commands pin the first node’s key fingerprint and carry a one-time, expiring token that authorizes the joining node. Subsequent requests authenticate with registered node keys. Revoking a node key immediately rejects its later requests.

## Prior art

These references inform the user experience and trust boundaries; they are patterns to borrow selectively, not a request to copy their transports or full feature sets.

### Remote viewing and recovery

- [Mosh](https://mosh.org/) is the closest match for unstable mobile links. Borrow its explicit stale/reconnected feedback and its model of synchronizing the latest screen state across loss and roaming. Mosh’s UDP SSP and predictive local echo target a full interactive terminal; Remuda’s first view instead uses short versioned sync requests and a line editor. Do not show speculative text as delivered: a line stays pending until the daemon acknowledges its idempotent batch.
- [Eternal Terminal](https://eternalterminal.dev/) resumes a byte stream using sequence numbers and buffered replay. Borrow the clear expectation that reconnect resumes the existing work. Reject transparent stream replay for input: a terminal write may have taken effect even when its acknowledgement was lost. Remuda retries a batch ID and shows `delivery uncertain` if deduplication can no longer establish the result.
- [tmate](https://github.com/tmate-io/tmate) provides distinct read-only and writable session share credentials; [Upterm](https://upterm.dev/docs/upterm.html) makes a host session easy to share and join. (Upterm’s `--read-only` flag applies to SFTP operations, so it is not evidence for terminal access modes.) Borrow visible access mode from tmate and a concise join flow from both. Reject public share links and relay-hosted sessions as the default: Remuda stays on the private VPN and authorizes named cluster nodes. A second viewer starts read-only; the host explicitly grants or transfers the writer lease.

### Cluster joining and identity

- [kubeadm join](https://kubernetes.io/docs/reference/setup-tools/kubeadm/kubeadm-join/) puts a bootstrap token and `--discovery-token-ca-cert-hash` pin in one copyable command. Borrow the one-line join UX and the rule that a token alone must not establish trust in an unknown server. Remuda pins the node fingerprint before using the one-time token; do not offer an unsafe skip-verification switch.
- [K3s](https://docs.k3s.io/quick-start) makes joining a node a concise install command with `K3S_URL` and `K3S_TOKEN`. Borrow combining installation and joining in one pasteable line. Reject an unscoped, long-lived shared cluster secret; Remuda’s bootstrap token is one-use and expires, then the registered node key identifies that node.
- [Tailscale auth keys and node keys](https://tailscale.com/docs/features/access-control/auth-keys) distinguish a provisioning credential from the key a node uses afterward, and surface key expiry/revocation. [Headscale pre-auth keys](https://headscale.net/stable/ref/registration/) also default to one use and a limited lifetime. Borrow separate one-time join credentials, node identity, expiry, and a visible revoke action. Do not assume VPN membership alone grants Remuda session access; the Remuda node registry still authorizes requests.
- [Nebula](https://nebula.defined.net/docs/guides/quick-start/) uses CA-signed host certificates with host names, addresses, and groups, plus lighthouses for discovery. Borrow human-readable node identities and explicit membership. Reject making a CA ceremony, lighthouse, NAT traversal, or discovery service a prerequisite: this first cluster assumes VPN reachability and pins its first node directly.
- [Syncthing](https://docs.syncthing.net/users/security) turns a certificate fingerprint into a human-friendly Device ID and requires peers to know/approve device identities; its [introducer](https://docs.syncthing.net/users/introducer.html) can propagate new devices. Borrow a short identity label backed by a fingerprint and a visible node list. Do not silently trust transitive introductions: every Remuda node join is explicit, and revocation names the affected node.
- [Magic Wormhole](https://magic-wormhole.readthedocs.io/en/latest/welcome.html) uses a one-time, human-sized PAKE code to establish a protected transfer. Borrow short-lived, single-use bootstrap material and clear expiry errors. Remuda’s join command also pins the first node’s fingerprint; do not rely on a short code alone to authenticate the cluster or add a public mailbox/relay service to this VPN-first UX.
- [WireGuard](https://www.wireguard.com/protocol/) demonstrates the Noise IK handshake with static peer keys; the [Noise Protocol Framework](https://www.noiseprotocol.org/) provides reviewed protocol patterns. Borrow established handshake primitives and pinned node keys. Do not invent cryptographic primitives or treat an encrypted channel as authorization: the request allowlist, node registry, and revocation checks remain required.
- [OpenSSH known_hosts and authorized_keys](https://man.openbsd.org/ssh) make host-key checking and an operator-managed authorized-key list familiar. Borrow a displayed, pinned fingerprint and a registry with explicit revocation. Reject SSH as the remote transport for this design, and never silently accept a changed key (TOFU); a mismatch stops the join with an actionable error.
