# Two-node cluster demo

Open two terminals with the ENABLE build on `PATH`. Paste this setup in both, choosing A's or B's directory at the last line:

```sh
D=/private/tmp/remuda-cluster-demo
umask 077
mkdir -p "$D/a" "$D/b"
use_node() {
  local node_dir=$1
  export HOME="$node_dir/home"
  export REMUDA_RUNTIME_DIR="$node_dir/runtime"
  export XDG_RUNTIME_DIR="$node_dir/xdg-runtime"
  export XDG_STATE_HOME="$node_dir/xdg-state"
  export XDG_CONFIG_HOME="$node_dir/xdg-config"
  export XDG_DATA_HOME="$node_dir/xdg-data"
  export XDG_CACHE_HOME="$node_dir/xdg-cache"
  mkdir -p "$HOME" "$REMUDA_RUNTIME_DIR" "$XDG_RUNTIME_DIR" \
    "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
  chmod 700 "$node_dir" "$HOME" "$REMUDA_RUNTIME_DIR" "$XDG_RUNTIME_DIR" \
    "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
}
use_node "$D/a" # Use "$D/b" in terminal B
```

Initialize A with `remuda -s node-a cluster init` and B with `remuda -s node-b cluster init`; note A's printed node name. On A enable input with `remuda -s node-a cluster control on`, then create a `demo` session with `remuda -s node-a -e 'remuda.session.new("demo", {"sh", "-c", "while IFS= read -r line; do printf '\''FROM_B:%s\\n'\'' "$line"; done"})'`. Configure the daemon listener on A with `remuda -s node-a cluster listen --bind 127.0.0.1:7441` and on B with `remuda -s node-b cluster listen --bind 127.0.0.1:7442`. Each command returns after the daemon reloads its listener; the daemon keeps listening. Add `--foreground` only when you want the command itself to hold the listener in that terminal. The invite in step 1 supplies the live fingerprint and one-line invite used in step 2.

1. On A, run `remuda -s node-a cluster invite --bind 127.0.0.1:7441`.
   Expected: A prints its node fingerprint and a one-line `remuda-join-v1` invite.
2. On B, run `remuda -s node-b cluster join 'FINGERPRINT' 'ONE-LINE-INVITE' --bind 127.0.0.1:7442`, substituting the fingerprint and invite from A.
   Expected: B prints `Joined NODE-A (fingerprint SHA256:...).` with A's fingerprint, then `Next: remuda cluster remote`.
3. On B, run `remuda -s node-b cluster remote NODE-A/demo`, substituting A's node name from `cluster init`.
   Expected: the TUI tree shows A and its live `demo` session selected.
4. In B's remote TUI, type `hello-from-b` and press Enter.
   Expected: the remote screen shows `FROM_B:hello-from-b` and `Input sent`.
5. On A, run `remuda -s node-a cluster revoke NODE-B --yes`, then on B run `remuda -s node-b cluster call NODE-A list --addr 127.0.0.1:7441 --json`.
   Expected: A reports the revoke and B exits with status 4, reporting that the peer refused the request.
