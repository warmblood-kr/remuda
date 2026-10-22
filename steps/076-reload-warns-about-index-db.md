# 076 — A reload warns when the target cannot read index.db

## Before

Reloading into an older build whose VFS index knows fewer schema steps
than this node's `index.db` went through silently. Afterwards every VFS
call failed with "index.db schema 3 is newer than this remuda (2)" until a
reload forward or `remuda vfs index-downgrade`. Nothing was lost, but
neither `reload` nor `reload --check` said so beforehand.

## Desired outcome

- The candidate's hello carries `index_db_steps`, the newest `index.db`
  schema it reads. The field is add-only: older daemons ignore it, and a
  hello without it reads as "unknown".
- The old daemon reads this node's `index.db` version read-only (no
  migration, no new file). When the target reads less, both `reload` and
  `reload --check` print `warning: index.db is at schema N but BUILD reads
  up to M; …` and name `remuda vfs index-downgrade M --yes`, or `remuda vfs
  export-index` when M is 0.
- A target from before the field gets a plain note when `index.db` exists.
- It is a warning, never a blocker: the check still says `ready:` and the
  reload goes through.

## Expected

`native/tests/reload_check.rs`:
- a fake older candidate (reads 2, index.db at 3): `ready:` plus the
  warning naming `index-downgrade 2`; a candidate with no field: the note;
- this build against an index.db at 1000: `--check` and the reload both
  warn, the reload goes through, and the session answers.
`native/src/reload.rs`: the warning text per case, and the hello field
read both ways.

## Actual

```
$ cargo test -p remuda-native --test reload_check
test result: ok. 8 passed; 0 failed

# red: with the warning dropped
test result: FAILED. 0 passed; 2 failed
```
