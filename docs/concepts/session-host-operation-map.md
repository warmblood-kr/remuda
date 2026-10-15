# Host operation mapping before adapter integration (#713)

The first foundations increment is inactive. It defines input descriptions,
physical delivery status and sequencing, not the complete RPC/result schema.
Do not integrate the adapter until the mappings below are serialized, retained
for lost-ACK recovery and tested against legacy behavior.

| Existing Session operation | Stable owner behavior/result |
| --- | --- |
| send(bytes) | One whole raw burst; retain write outcome |
| send_line(text) | Whole type/submit act with zero settle; retain Late/error mapping |
| input_text(text) | Normalize/bracket paste, retain pending tail/baseline; no Return |
| type_text(text, settle) | Whole normalized body, settle, Return/observation; retain Submitted/Unverified/Late and errors |
| submit(expect) | Consume host pending tail, retain Submitted/Unverified/Late and errors |
| clear_input(key) | Host human gate/allowlist/rate check; retain Option<String> return and errors |
| feed(steps) | One reservation across every burst/pause; existing Session 5-second total pause cap |
| apply_input_batch | Host instance/client/sequence, payload binding, rate budget and retained outcome |
| attach/takeover | Host attachment generation and human gate; authenticated controller epoch also required |
| attached write_raw | Whole burst under valid host attachment generation; no fresh client-owned attachment state |
| resize/terminate | Atomic mutation reservation, immutable instance, current epoch; explicit close only |
| input status/busy/pending | Stable owner reports real pending worker/follow-up, never inferred from RPC timeout |
| snapshot/cursor/mouse/history | Host parser observations and immutable versioned snapshot cut |
| is_alive/exit_info/owns_process | Host sole reaper/identity; unavailable owner is distinct from child exit |

Delivery status Written is not the submission result and is not evidence the
child executed input. In particular, a Late submit result does not release the
physical reservation while its delayed Enter can still land. Full operation
results must be retained alongside the delivery ledger before runtime adoption;
the current outcome-only ledger is not a complete implementation of that RPC.
Returned text and error payloads need explicit byte budgets and oversized-result
semantics; do not silently truncate a successful result and replay after lost ACK.

The runtime must not construct a second independent Session safety owner around
a remote PtyAgent. Introduce a high-level Session operations/backend seam while
keeping legacy operations local. Route one complete act to host and preserve its
identity/result across controller replacement. Begin/finish tokens reserve all
effects, including resize/close; the opaque ActId prevents a duplicate completion
from releasing a later act in the same controller epoch.

Stable client registration is a lifetime cap of 16 per host incarnation. A
replacement controller reuses durable stable client IDs; creating fresh IDs on
every reconnect would exhaust the cap. At capacity, report ClientLimit and
require reuse of an existing ID or planned new session; never clear history or
silently recycle clients to make room.
