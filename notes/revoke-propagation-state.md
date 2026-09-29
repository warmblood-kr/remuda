Branch: feat/cluster-replication-wiring
SHA: 4a9814ef652b1c713e0946670671fc68145c287a
Done: Bounded five-second CLI push, daemon anti-entropy pull, exact A/B/C fast and fallback tests, and joined anti-entropy worker shutdown.
Next: dev-lead SEC review and qa3 validation on this branch.
Open risks: SEC and qa3 are pending; peers without known endpoints rely on their periodic pull to catch up.
