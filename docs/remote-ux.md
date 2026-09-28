# Remote session UX draft

Remote mode continues the existing Remuda TUI: the list stays on the left and the selected session stays on the right. The difference is that the left side becomes a virtual tree of nodes and their sessions, addressed as `node/session`. The selected remote screen keeps refreshing over short requests and survives network loss. There is no separate remote-client look. The name `control tower` remains reserved for a later composed multi-node control experience.

The approved command tree is `remuda cluster init`, `remuda cluster join <line>`, `remuda cluster nodes`, `remuda cluster revoke <node>`, and `remuda cluster remote [node/session]`. These subcommands are not implemented yet. Example addresses, fingerprints, tokens, and output are illustrative.

## Journey 0: form a cluster

On the first machine, initialize a cluster of one. `remuda cluster init` creates the node key pair and starts the cluster endpoint on the VPN address. Bare `remuda cluster` shows cluster status after initialization, or the init hint when no cluster exists. Initialization prints the address, public-key fingerprint, and a short-lived, single-use join line:

```text
$ remuda cluster init
Cluster initialized
Node: studio
Address: 100.80.0.12:7443
Fingerprint: SHA256:QmFzZTY0LWZpbmdlcnByaW50LWV4YW1wbGU=
Join line (expires in 10 minutes; single use):
remuda-join://100.80.0.12:7443?fingerprint=SHA256%3AQmFzZTY0LWZpbmdlcnByaW50LWV4YW1wbGU%3D&token=eyJleGFtcGxlLW9uZS10aW1lLXRva2Vu
```

The operator shares the join line with the intended machine over a trusted channel. The line is a bearer secret: it is shown once, expires quickly, and is consumed once. On the joining machine, run `remuda cluster join <line>` with the received line. The first node's fingerprint is pinned before the secure channel is established, then the joiner registers its own public key. No public discovery or NAT traversal is implied; both nodes are expected to reach one another on the same VPN.

```text
$ remuda cluster join 'remuda-join://100.80.0.12:7443?fingerprint=SHA256%3AQmFzZTY0LWZpbmdlcnByaW50LWV4YW1wbGU%3D&token=eyJleGFtcGxlLW9uZS10aW1lLXRva2Vu'
Pinned cluster node fingerprint verified
Joining cluster…
Joined cluster as node: field-laptop
```

Every member knows the public keys of every cluster member through a replicated, signed `authorized_nodes` list. Any member can admit or revoke a node; membership changes are pushed to peers, and nodes fetch the current list at startup. Operators can inspect membership and revoke a node key from any member:

```text
$ remuda cluster nodes
NODE          ADDRESS          STATUS
studio        100.80.0.12:7443 online
field-laptop  100.80.0.27:7443 online

$ remuda cluster revoke field-laptop
Revoke node field-laptop (SHA256:Vmlld2VyLWtleS1leGFtcGxl)? [y/N] y
Node field-laptop revoked. Existing connections closed.
```

The list formatting and confirmation syntax are illustrative. The join line is shell-quoted in this example; a PowerShell host should show equivalent quoting. A token or fingerprint error is fatal and must not fall back to an unpinned connection; unreachable nodes can be retried after the VPN is repaired:

```text
Join failed: token expired or already used. Run `remuda cluster init` on the first node to create a new join line.
Join failed: fingerprint mismatch for 100.80.0.12:7443. Expected SHA256:QmFz...; received SHA256:YW5vdGhlci1rZXk. Join aborted; verify the command with the cluster operator.
Join failed: 100.80.0.12:7443 is unreachable. Check that both machines are on the VPN, then retry the join command.
```

## User journeys

### 1. Host a session and open the cluster view

On the host node, start the session normally:

```sh
remuda run -n dev claude
```

From an already joined node, run `remuda cluster remote` to open the same Remuda TUI with the cluster tree on the left and selected-session screen on the right. `remuda cluster remote studio/dev` opens that same UI with the target selected. The daemon keeps the session alive when the initiating terminal goes away.

At first open, expand the local/current node and collapse other nodes. Use ↑/↓ to move through the tree, ←/→ to collapse or expand node groups, and Enter to select a session and focus its line composer. The current target is always named `node / session` in the right pane. A terminal snapshot is shown there with its capture age; the line composer remains in the familiar Remuda footer area.

### 2. Phone or laptop view on an unstable network

From Termux or a laptop joined to the cluster:

```sh
remuda cluster remote
```

Use the same tree and keyboard model on each platform. Node groups collapse, the current node starts expanded, and other nodes start collapsed. Press `/` to search node/session names; `!` optionally limits the list to sessions needing attention (stale, ended, or with pending input). The selected node and session stay visible while navigating. Selecting `phone / dev` shows its captured screen and an input line; this is not a separate remote app or share-link flow.

### 3. Network drop and resume

If one node drops off the VPN, keep its last screen visible and show its reachability and snapshot age in both the tree and right pane. Continue composing text locally. Enter freezes the entire line as a batch; show each pending line until acknowledged. On recovery, resume from the last screen version and retry each batch with its original ID.

```text
remuda · cluster (2/3 reachable)
> ▼ studio       local · reachable · sync 1s
    dev          live
  ▶ field-laptop unreachable · last sync 28s
  ▶ buildbox     reachable · sync 2s
─────────────────────────────────────────────────────────
field-laptop / dev · stale snapshot · reconnecting
$ Please check the migration output

Input pending · 1 line · retrying with same batch ID
```

A line stays in the draft until Enter. Once submitted, it remains in the pending queue until the daemon acknowledges it. A timeout retries the exact batch ID and bytes; it never creates a new batch for the same line. If deduplication history cannot prove whether the batch applied, show `delivery uncertain` and do not silently send it again under a new ID.

### 4. Two viewers send input

Two viewers can select the same session and each can send lines. Each Enter submits one atomic batch. The host applies complete batches in arrival order, so concurrent batches can be adjacent but never interleave character by character. A small transient hint may say `input from field-laptop` or `input from operator`; input does not wait for exclusive ownership.

### 5. Switch sessions or nodes

Move to another node group or session in the left tree and select it. The right pane changes its target label to the selected `node / session`; no separate command or view is needed. A nonempty local draft prompts to keep or discard it. Submitted pending batches stay associated with their original node/session and are never retargeted. Search with `/`; optionally press `!` to show only sessions needing attention.

### 6. Detach or terminate

**Detach** means leave the remote TUI; the remote session keeps running. With list/tree focus, press `q` to leave, with no confirmation. Keep any unsent draft locally with its node/session target. When a session has focus, use the existing `Ctrl-\` to return to the list, then `q` to detach. In the line composer, `q` is ordinary draft text; termination is only available when the tree has focus.

**Terminate** means kill the session process. It uses the same `x` key and confirmation as local Remuda: select the live session, press `x`, then confirm `y` at `kill dev? it is running — y / n`. Do not add a remote-only key or map Esc, `q`, or ordinary input to termination. If an ended session is selected, show its final available screen and ended status; the existing `x` behavior for an exited row clears it from the list.

After the process exits, retain its selected final screen/status long enough to identify it as ended, then reflect removal from the remote session list. Any unsent draft stays local and must not go to a later session that reuses the same name.

## Screen states

The same Remuda TUI layout is used in every state: virtual node/session tree on the left, selected screen on the right. Nodes are collapsible groups. Expand the local/current node by default and collapse the rest. Show each node’s reachable/unreachable badge and last-sync age; show session status and pending-input markers on child rows. `/` searches names; optionally `!` filters to sessions needing attention.

### All nodes up

```text
remuda · cluster (3/3 reachable)
> ▼ studio       local · reachable · sync 1s
    dev          live
  ▶ field-laptop reachable · sync 2s · 2 sessions
  ▶ buildbox     reachable · sync 1s · 4 sessions
──────────────────────────────────────────────────────
studio / dev · live · snapshot 1s ago
$ Waiting for test results…

> Type a line; Enter sends · Ctrl-\ list · q detach
```

### One node unreachable

```text
remuda · cluster (2/3 reachable)
> ▼ studio       local · reachable · sync 1s
    dev          live
  ▶ field-laptop unreachable · last sync 28s
  ▶ buildbox     reachable · sync 2s
──────────────────────────────────────────────────────
field-laptop / dev · stale snapshot · reconnecting
$ Waiting for test results…

Node unreachable. Keeping last screen; retrying…
```

### Stale snapshot while recovering

```text
remuda · cluster (2/3 reachable)
> ▼ studio       local · reachable · sync 1s
    dev          live
  ▼ field-laptop reconnecting · last sync 28s
    dev          stale · pending input 1
  ▶ buildbox     reachable · sync 2s
──────────────────────────────────────────────────────
field-laptop / dev · stale · snapshot 28s ago
$ Waiting for test results…

Input pending · 1 line · retrying with same batch ID
```

### Ended session

```text
studio / dev · ended · final snapshot 2s ago
$ Process exited (status 0). Input is disabled.
x clears ended session · q detaches
```

Other actionable failures name the target and next step: `field-laptop / dev · access denied — ask the cluster operator`; `field-laptop / dev · session not found — check the session name`; `field-laptop · unreachable — check VPN, retrying`; `field-laptop / dev · delivery uncertain — batch <id> may have been applied; do not resend as a new line`.

## Input composition and special keys

The selected session pane stays a snapshot, with the line editor integrated into the existing TUI footer. Text, cursor movement, Backspace, Delete, and Home/End edit the local draft. Enter sends the whole line as one idempotent batch. Show each submitted line with `waiting to send`, `sending`, or `sent` acknowledgement status. Editing after Enter creates a new draft and never changes a submitted batch.

While offline, characters accumulate in the current draft. Enter freezes that line into the pending queue; pending batches preserve local submission order and retry with the same IDs and bytes. Two viewers’ batches are atomically applied in daemon arrival order. A timeout or reconnect never changes the batch ID. If a batch cannot be proven applied or unapplied, report uncertainty rather than silently duplicating it. Show a small `input from <node/user>` hint when a remote batch arrives, without assigning exclusive input ownership.

The remote line editor handles ordinary text, not arbitrary terminal key timing. Arrows edit the local line; Ctrl-C cancels its current draft; Ctrl-\ returns from session focus to the list; `q` from list/tree focus detaches from the TUI; `x` on the selected session invokes the same termination confirmation as local Remuda. Special application keys remain outside this line-oriented first UX.

## Open UX decision

The tree may offer an attention-only filter (`!`) in addition to `/` name search. Recommend including it if the existing list footer has room; stale nodes, ended sessions, and pending input are the attention states. It does not change the default view: local/current node expanded, other node groups collapsed.

## Protocol implications

Sync returns the selected session’s latest snapshot plus an output version; a reconnect resumes from the last version. Enter submits `input(session, batch_id, bytes)`, and retries reuse the same ID and bytes. The host deduplicates each viewer’s batch and applies whole batches atomically in arrival order. Local calls reuse the daemon socket; remote calls use Remuda’s authenticated short request/response channel over the VPN, with joined node keys and pinned peers. The channel uses off-the-shelf Noise IK via `snow`; a per-session output event from #190 can later wake sync without changing the TUI.

### Security and implementation constraints

- The allowlisted request front comes first, initially on a local socket. It exposes only the operations required by this UX; never forward the general daemon protocol or arbitrary Lua remotely. The network listener is a later layer over that restricted front.
- If the listener needs TCP, allow exactly one scoped clippy TCP-ban exception in its module (`#[allow]` at that module), with a `clippy.toml` and documentation note that names #191. Bind only to the configured VPN address; never default to `0.0.0.0`. Start the listener only after `remuda cluster init`.
- Join commands pin the first node’s key fingerprint and carry a one-time, expiring token that authorizes the joining node. Subsequent requests authenticate with registered node keys. Revoking a node key immediately rejects its later requests.

## Prior art

These references inform the user experience and trust boundaries; they are patterns to borrow selectively, not a request to copy their transports or full feature sets.

### Remote viewing and recovery

- [Mosh](https://mosh.org/) is the closest match for unstable mobile links. Borrow its explicit stale/reconnected feedback and its model of synchronizing the latest screen state across loss and roaming. Mosh’s UDP SSP and predictive local echo target a full interactive terminal; Remuda’s first view instead uses short versioned sync requests and a line editor. Do not show speculative text as delivered: a line stays pending until the daemon acknowledges its idempotent batch.
- [Eternal Terminal](https://eternalterminal.dev/) resumes a byte stream using sequence numbers and buffered replay. Borrow the clear expectation that reconnect resumes the existing work. Reject transparent stream replay for input: a terminal write may have taken effect even when its acknowledgement was lost. Remuda retries a batch ID and shows `delivery uncertain` if deduplication can no longer establish the result.
- [tmate](https://github.com/tmate-io/tmate) provides distinct read-only and writable session share credentials; [Upterm](https://upterm.dev/docs/upterm.html) makes a host session easy to share and join. (Upterm’s `--read-only` flag applies to SFTP operations, so it is not evidence for terminal access modes.) Borrow a concise join flow from both. We considered separate read-only and writable sharing, but the approved Remuda UX lets every authorized viewer submit lines; show only a transient `input from <node/user>` hint. Reject public share links and relay-hosted sessions as the default: Remuda stays on the private VPN and authorizes named cluster nodes.

### Cluster joining and identity

- [kubeadm join](https://kubernetes.io/docs/reference/setup-tools/kubeadm/kubeadm-join/) puts a bootstrap token and `--discovery-token-ca-cert-hash` pin in one copyable command. Borrow the one-line join UX and the rule that a token alone must not establish trust in an unknown server. Remuda pins the node fingerprint before using the one-time token; do not offer an unsafe skip-verification switch.
- [K3s](https://docs.k3s.io/quick-start) makes joining a node a concise install command with `K3S_URL` and `K3S_TOKEN`. Borrow combining installation and joining in one pasteable line. Reject an unscoped, long-lived shared cluster secret; Remuda’s bootstrap token is one-use and expires, then the registered node key identifies that node.
- [Tailscale auth keys and node keys](https://tailscale.com/docs/features/access-control/auth-keys) distinguish a provisioning credential from the key a node uses afterward, and surface key expiry/revocation. [Headscale pre-auth keys](https://headscale.net/stable/ref/registration/) also default to one use and a limited lifetime. Borrow separate one-time join credentials, node identity, expiry, and a visible revoke action. Do not assume VPN membership alone grants Remuda session access; the Remuda node registry still authorizes requests.
- [Nebula](https://nebula.defined.net/docs/guides/quick-start/) uses CA-signed host certificates with host names, addresses, and groups, plus lighthouses for discovery. Borrow human-readable node identities and explicit membership. Reject making a CA ceremony, lighthouse, NAT traversal, or discovery service a prerequisite: this first cluster assumes VPN reachability and pins its first node directly.
- [Syncthing](https://docs.syncthing.net/users/security) turns a certificate fingerprint into a human-friendly Device ID and requires peers to know/approve device identities; its [introducer](https://docs.syncthing.net/users/introducer.html) can propagate new devices. Borrow a short identity label backed by a fingerprint and a visible node list. Do not silently trust transitive introductions: every Remuda node join is explicit, and revocation names the affected node.
- [Magic Wormhole](https://magic-wormhole.readthedocs.io/en/latest/welcome.html) uses a one-time, human-sized PAKE code to establish a protected transfer. Borrow short-lived, single-use bootstrap material and clear expiry errors. Remuda’s join command also pins the first node’s fingerprint; do not rely on a short code alone to authenticate the cluster or add a public mailbox/relay service to this VPN-first UX.
- [WireGuard](https://www.wireguard.com/protocol/) demonstrates the Noise IK handshake with static peer keys; the [Noise Protocol Framework](https://www.noiseprotocol.org/) provides reviewed protocol patterns. Borrow established handshake primitives and pinned node keys. Do not invent cryptographic primitives or treat an encrypted channel as authorization: the request allowlist, node registry, and revocation checks remain required.
- [OpenSSH known_hosts and authorized_keys](https://man.openbsd.org/ssh) make host-key checking and an operator-managed authorized-key list familiar. Borrow a displayed, pinned fingerprint and a registry with explicit revocation. Reject SSH as the remote transport for this design, and never silently accept a changed key (TOFU); a mismatch stops the join with an actionable error.

### Tree navigation prior art

For long lists, borrow the hierarchy of [tmux `choose-tree`](https://github.com/tmux/tmux/wiki/Getting-Started/86df5fe449a2d0499cf47a7e16245a3c6d6562d5), which groups sessions/windows/panes and supports collapsed branches; borrow context awareness and search from [k9s](https://k9scli.io/topics/commands/), the host tree pattern from [VS Code Remote Explorer](https://code.visualstudio.com/docs/remote/ssh), and reachability/last-seen filtering from the [Tailscale device list](https://tailscale.com/docs/features/access-control/device-management/how-to/filter). Recommendation: one collapsible node group per host, sessions nested beneath it, local/current node expanded by default and other groups collapsed. Keep the selected session preview on the right in the existing Remuda layout; show reachability and last-sync age on each node row. `/` searches names and `!` filters for attention states. Do not make nodes mutually exclusive contexts that hide the rest of the cluster.
