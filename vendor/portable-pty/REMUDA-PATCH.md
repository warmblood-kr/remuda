# Pinned portable-pty source and optional creation-time Job seam

Upstream package: portable-pty 0.9.0, MIT (LICENSE.md retained).
Crates.io archive SHA256: b4a596a2b3d2752d94f51fac2d4a96737b8705dddd311a32b9af47211f08671e.
Archive VCS metadata: wezterm/wezterm commit f8921727a11b9f8b073e8c24821d72fd41283500, path pty (see .cargo_vcs_info.json).
Source copied from the downloaded registry package; Cargo.toml is the published normalized manifest, Cargo.toml.orig retained as provenance. The local Cargo.lock/registry checksum bookkeeping are not source inputs.

The only intended behavioral addition is an opt-in Windows `remuda-job-list` feature. SlavePty gets a fail-closed `spawn_command_in_job(CommandBuilder, BorrowedHandle)` method; ConPTY implements it using a second STARTUPINFOEX attribute (JOB_LIST) in the existing CreateProcessW call. Other backends refuse. Ordinary spawn_command still passes no Job attribute. Unix code and command quoting/env/cwd remain upstream. The Job and its HANDLE array live through attribute-list deletion; no Job handle is inherited and no spawn-then-assign/uncontained fallback is used. Feature selection does not enable the experimental session host on Windows.

Microsoft contract: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute . A caller must retain its noninheritable configured Job for the intended child lifetime and must check the returned real process handle. Incompatible enclosing Job restrictions are errors. Native Windows containment/cleanup tests are required; cross compilation is insufficient.

Known upstream WinChild wait/kill/Future defects are deliberately not silently fixed in the default path by this patch. The experimental host must use its checked retained-process-handle wrapper rather than upstream kill/try_wait/wait/Future for lifecycle proof. This vendor patch can be removed after an upstream release provides the equivalent creation hook and passes the host's native acceptance tests.
