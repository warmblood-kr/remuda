# Two-node cluster demo

Use two terminals with the same ENABLE build and separate private runtimes. Replace `$D` with a private directory under `/private/tmp`; create `$D/a` and `$D/b` with mode `700`, each with private `home`, `runtime`, `xdg-runtime`, `xdg-state`, `xdg-config`, `xdg-data`, and `xdg-cache` directories. In each terminal export `HOME`, `REMUDA_RUNTIME_DIR`, and the XDG variables to that node's corresponding directories, then use `remuda -s node-a` in A and `remuda -s node-b` in B. Initialize both nodes with `cluster init`; note A's printed node name. On A enable input with `cluster control on`, then create a `demo` session with `remuda -s node-a -e 'remuda.session.new("demo", {"sh", "-c", "while IFS= read -r line; do printf '\''FROM_B:%s\\n'\'' "$line"; done"})'`. Start `cluster listen --bind 127.0.0.1:7441` on A and `cluster listen --bind 127.0.0.1:7442` on B, each in its own terminal, and keep them running. The invite in step 1 supplies the live fingerprint and one-line invite used in step 2.

1. On A, run `remuda -s node-a cluster invite --bind 127.0.0.1:7441`.
   Expected: A prints its node fingerprint and a one-line `remuda-join-v1` invite.
2. On B, run `remuda -s node-b cluster join 'FINGERPRINT' 'ONE-LINE-INVITE' --bind 127.0.0.1:7442`, substituting the fingerprint and invite from A.
   Expected: B prints `Joined cluster.`
3. On B, run `remuda -s node-b cluster remote NODE-A/demo`, substituting A's node name from `cluster init`.
   Expected: the TUI tree shows A and its live `demo` session selected.
4. In B's remote TUI, type `hello-from-b` and press Enter.
   Expected: the remote screen shows `FROM_B:hello-from-b` and `Input sent`.
5. On A, run `remuda -s node-a cluster revoke NODE-B --yes`, then on B run `remuda -s node-b cluster call NODE-A list --addr 127.0.0.1:7441 --json`.
   Expected: A reports the revoke and B exits with status 4, reporting that the peer refused the request.
