# PR #294 review

Reviewed commit `c202e1296664a35dd9ae50e0585eca83c5776f9e` against `c9b9380`.

## Finding

- **High: `create_or_open_lock` can create the lock without its protected DACL on the fallback path.** It first uses `CREATE_NEW` with owner-only `SECURITY_ATTRIBUTES`, then, after `AlreadyExists`, retries with `OPEN_ALWAYS` and a null security descriptor (`native/src/cluster/windows_security.rs:231-268`). If another process removes the existing lock between those calls, `OPEN_ALWAYS` creates a new file using the default/inherited ACL. `StateLock::acquire` only tightens that ACL after the handle is returned (`native/src/cluster/storage.rs:133-137`), leaving a create-to-tighten window. Use `OPEN_EXISTING` for the fallback and retry `CREATE_NEW` if it vanished, or otherwise ensure every creation uses the owner-only descriptor.

## Review checks

- Windows state reads for identity, registry, join tokens, control settings, and registry revision use `open_for_check` plus handle-based `check_private_file`; directories and lock files also go through handle-based verification/upgrading.
- New directories, state files, lock files, and atomic-write temporary files receive the owner-only protected DACL at creation. Atomic replacements carry that descriptor with the file.
- Existing objects are opened with `FILE_FLAG_OPEN_REPARSE_POINT` and rejected if the handle identifies a reparse point. I did not find a direct production read/write bypass in the cluster state paths.
- Target checks passed: `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets` and `cargo test --target x86_64-pc-windows-gnu --workspace --all-targets --no-run`. These cross-compiled the tests; I could not execute Windows binaries on this macOS host.
- Unix paths retain the existing `O_NOFOLLOW`, uid and mode checks, and `0600`/`0700` creation behavior.

The owner/SID migration edge cases, child ACL inheritance, and missing reparse/extra-ACE tests were reported separately by dev-lead and are not duplicated here.

## Re-check: `6ebd547000cfcbdbf2626f8310e0c813c994d940`

- **PASS: lock creation race fixed.** `create_or_open_lock` now retries `CREATE_NEW` with the owner-only descriptor if the existing lock disappears between `AlreadyExists` and `OPEN_EXISTING`; the fallback no longer creates a file with inherited ACLs.
- The follow-up also handles token-owner legacy files, secures known children before tightening the parent directory, and adds the unrelated-owner, extra-ACE, and reparse-point checks from the earlier review notes. I found the earlier migration/child ACL concerns addressed in this patch.
- Unix code paths are unchanged: the diff is confined to Windows-gated implementations, while existing Unix `O_NOFOLLOW`, uid/mode validation, and private creation modes remain intact.
- `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets` and `cargo test --target x86_64-pc-windows-gnu --workspace --all-targets --no-run` both pass on this host. Windows test binaries were cross-compiled but not executed here.
- **Verdict: PASS** for the requested re-check.
