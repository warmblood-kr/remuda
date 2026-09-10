# 019 — a gate for dependency advisories

## Before

No CI job runs `cargo audit` against this repo. The advisory feed
(RustSec) can turn a clean dependency tree red on its own schedule, with no
commit of ours, and nothing here would notice.

Measured before touching anything: plain `cargo audit` (no flags) exits `0`
even when an advisory is present, because it only *reports* `unmaintained` /
`unsound` / `yanked` findings by default — it does not fail the build on
them. Only `--deny warnings` does. A gate wired as bare `cargo audit` would
be theatre: green on the exact thing it exists to catch.

## Desired outcome

1. A `cargo-audit` job in `ci.yml`, same shape as the other gates in this
   file, running `cargo audit --deny warnings` so `unsound` / `unmaintained`
   / `yanked` findings fail the build, not just `vulnerabilities`.
2. Crate count and finding count (vulnerabilities, warnings) printed to
   `$GITHUB_STEP_SUMMARY` on every run, including a clean one — so a human
   can see the zero without a red ever having happened, and notice the day
   it stops being zero.
3. The workflow comment states the runbook for a future red: read the
   advisory and bump the crate — never just re-run the job, since a red can
   arrive from RustSec publishing overnight with no commit of ours. And for
   the no-patched-version case (`unmaintained` advisories often have none):
   replace the crate, or `--ignore RUSTSEC-XXXX-NNNN` with a comment naming
   a dated expiry — never ignore with no expiry.

Sequencing (given by 정수님): fix any existing finding first, then turn the
gate on — so it lands green on day one and every future red is genuinely
new. This repo's own `Cargo.lock` was already clean (checked below), so
there was nothing to fix here; the fix-then-gate order applied to
`monocle-cli` in the same batch of work, where an `event-listener` advisory
was present.

## Expected

- `cargo audit --json` against this repo's `Cargo.lock` reports
  `dependency-count: 104`, `vulnerabilities.count: 0`, `warnings: {}`.
- `cargo audit --deny warnings` exits `0` on this repo's current lockfile.
- The same command, run against a lockfile with a known-vulnerable crate
  pinned (`time = "=0.1.44"`, RUSTSEC-2020-0071), exits non-zero — proving
  the flag the gate depends on actually discriminates, not just that the
  tool runs.
- `python3 scripts/check-workflows.py` still reports all workflow files
  parsing, with one more job than before.

## Actual

```
$ cargo audit --json | jq '.lockfile."dependency-count", .vulnerabilities.count, .warnings'
104
0
{}

$ cargo audit
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
      Loaded 1243 security advisories (from ~/.cargo/advisory-db)
    Updating crates.io index
    Scanning Cargo.lock for vulnerabilities (104 crate dependencies)
$ echo exit=$?
exit=0
```

Positive control — proves `--deny warnings` actually gates, not just that
`cargo audit` runs (throwaway crate, `time = "=0.1.44"`, not part of this
repo):

```
$ cargo audit --deny warnings
    Scanning Cargo.lock for vulnerabilities (7 crate dependencies)
Crate:     time
Version:   0.1.44
Title:     Potential segfault in the time crate
Date:      2020-11-18
ID:        RUSTSEC-2020-0071
URL:       https://rustsec.org/advisories/RUSTSEC-2020-0071
Severity:  6.2 (medium)
Solution:  Upgrade to >=0.2.23

error: 1 vulnerability found!
$ echo exit=$?
exit=1
```

Negative control on this repo's own lockfile, same flags as the CI job:

```
$ cargo audit --no-fetch --deny warnings
    Scanning Cargo.lock for vulnerabilities (104 crate dependencies)
$ echo exit=$?
exit=0
```

```
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 15 job(s) defined
```

## Not verified

- **The job running inside actual GitHub Actions.** `taiki-e/install-action`
  fetching `cargo-audit` and `$GITHUB_STEP_SUMMARY` rendering were not
  observed on a real runner — only the underlying `cargo audit` commands
  were run locally, against the advisory DB as of 2026-09-10 (1243
  advisories). The PR's own CI run is the first real execution.
- **A future `unmaintained` advisory with no patched version.** The
  `--ignore RUSTSEC-XXXX-NNNN` + dated-expiry runbook is documented but has
  no worked example here, since no such advisory exists against this repo
  today.
