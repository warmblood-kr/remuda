# Session host protocol foundations (#713)

This is an inactive internal milestone. It does not create a host, authenticate
IPC, preserve a session or change launch/shutdown behavior. The complete design
and outstanding acceptance gates are in https://github.com/warmblood-kr/remuda/issues/713.

A future stable host owns `core::Session`, PTY/ConPTY, parser, input safety,
logical acts, reaping and Windows Job. The replaceable daemon owns policy and
cluster routing. Adoption is for new sessions; existing daemon-owned sessions
need their original owner until drained. Host replacement and live migration
are separate work. No production restart is part of this milestone.

## Wire

The serialized contract has RMHS magic, major 1/minor 0 and required capability
negotiation. Unknown major or required capability refuses before mutation.
Length-prefixed frames have a four-byte big-endian nonzero length, checked
before payload allocation: 4 KiB handshake and 64 KiB absolute maximum.
Runtime must add private authenticated IPC, fail-closed peer identity checks,
connection caps and deadlines; framing alone cannot protect a blocking reader.
Rust traits are build-internal, not a versioned ABI.

`InputAct` describes a complete operation: bytes, send-line, type-text, submit,
clear or feed. Its budget is 8 KiB aggregate input, 64 steps and the existing Session 5-second cap on
aggregate pause. Runtime must execute the entire reservation in host, including
delayed Enter and timeout-running workers. Typed normalized serialization,
including operation kind, is SHA-256 bound to stable client/sequence identity.
The host computes this digest; supplied digest bytes are not authority.

## Controller and input outcomes

Each effect also has a nonwrapping opaque ActId; duplicate completion cannot
release a later act. A controller fence contains host incarnation, authenticated connection identity
and monotonic epoch. Acquisition never wraps. On disconnect/release/expiry,
started work enters Draining. No new controller is granted until the whole act
resolves, including pauses and follow-ups. Read recovery remains possible.
An indefinitely blocked OS write can indefinitely prevent handover; falsely
claiming exclusive new ownership would allow old input to land afterward.
Runtime must use act reservation for every mutation, including resize/close.

Ledger identity does not include controller epoch. Sixteen client identities
are retained for the host incarnation and never evicted; registration at
capacity refuses. Each client retains 256 outcomes. Positive sequences must be
consecutive; evicted older sequences return Forgotten and cannot replay.
Retained sequence reuse with another digest conflicts. Pending entries cannot
be evicted; a second sequence while pending is Busy. Sequence exhaustion refuses.
This is bounded host memory, not persistence through host failure.

Terminal outcomes are Rejected (safe before effect), Written (PTY write/flush
completed) and Indeterminate. Written does not mean application execution.
Request timeout does not mean completion/cancellation. Runtime must retain and
query the old operation identity after lost ACK; never replay unknown outcomes.

## Subscriber state

Each subscription starts NeedsResync. One immutable snapshot cut enters
AwaitAck(snapshot ID, generation). Deltas identify their base generation and
remain bounded even while ACK is pending: at most 32 items and 256 KiB.
Snapshot size is capped at 32 MiB; IDs never wrap. Overflow, a generation gap or
bad order invalidates snapshot and queue, returning NeedsResync. A stale ACK
cannot restore the old baseline. Only after valid ACK can queued updates drain.

These are state-machine bounds, not runtime memory/fairness claims. Runtime
must cap subscribers, simultaneous snapshot copies/builds and aggregate hosts;
capture a coherent parser cut; chunk transfer; bind ACK to incarnation and
subscription; cancel expired/disconnected consumers. Never hold parser locks
during socket writes. Existing raw subscriber EOF means process death and must
not be reused as a silent overflow signal.

## Remaining release gates

The isolated usable host must prove child/session identity, parser state,
geometry, terminal query replies and input outcomes through controller death
and replacement on macOS/Linux/Windows. Host authentication, full Session
operations seam, explicit exit/EOF/drain status, bounded OSC/DCS, cancellable
adapter monitors, birth-environment caller attribution, Windows spawn/job
assignment race, cross-version cluster behavior and executable retention remain
open. Same-UID code is not isolated by user-readable capabilities. No zero-loss
or exactly-once application execution guarantee is claimed.
