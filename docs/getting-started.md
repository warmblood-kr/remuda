# Getting started

From nothing to a Butler you can reach from your phone, on three machines.
You need your own computer (macOS, Windows or Debian), your own Matrix
account, and Element on your phone.

## 1. Install Remuda and the Butler

macOS (Apple Silicon) or Debian (x86_64):

```sh
curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | REMUDA_CHANNEL=nightly REMUDA_INSTALL_BUTLER=1 sh
```

Windows (x86_64), in PowerShell:

```powershell
$env:REMUDA_CHANNEL='nightly'; $env:REMUDA_INSTALL_BUTLER='1'; irm https://warmblood-kr.github.io/remuda/install.ps1 | iex
```

You should see, as the last line, `Next: remuda butler doctor`.
If not: nightly has no Intel Mac or ARM Linux binary; build from source
there. Remuda lands in `~/.local/bin`; make sure that directory is on `PATH`.

## 2. Check your coding agents

```sh
remuda butler doctor
```

You should see:

```text
Claude Code: installed, logged in
Codex CLI: installed, logged in
Next: remuda butler matrix setup
```

If not: run the `Next:` line it prints (for example `claude auth login` or
`codex login`), then run `remuda butler doctor` again.

## 3. Connect the Butler to Matrix

```sh
remuda butler matrix setup
```

The wizard asks four things:

1. `Matrix homeserver URL:`, for example `https://matrix.example.org`.
2. `Your Matrix user ID (for example @alice:example.org):`, your own account.
3. For an `https://` homeserver, `HTTPS trust: enter a 64-character SHA-256
   certificate pin or an absolute CA file path:`
   - Self-signed certificate: the pin. Compute it from the server's
     certificate and copy the 64 hex digits after `=`:

     ```sh
     openssl x509 -in server-cert.pem -pubkey -noout | openssl pkey -pubin -outform DER | openssl dgst -sha256
     ```

   - Private CA: the absolute path of the CA file.
   - A certificate from a public CA: TODO(lead): setup has no flag-free path;
     HTTPS always needs a pin or a CA file. Say which one to use here.
4. A summary, then `Continue? Type Y to continue, or N to cancel [N]:`. Type `Y`.

The wizard creates a bot account, so it then asks for the server's
registration token (hidden). Get it from the homeserver admin.

You should see, as the last line:

```text
Next: accept the invite in Element; the relay is running, so write to the Butler there.
```

If not, follow the `Next:` line it prints: on a pin mismatch, recompute the
pin; if registration is disabled, ask the admin for a bot account.

Rooms are open: anyone can invite the Butler. The sender allowlist decides
trust; it starts with only you, and other senders' messages are quarantined.

## 4. Talk to it from your phone

Open Element on your phone, signed in as the user ID from step 3. Accept the
room invite from the bot, then type a message, for example:

```text
hello, what are you running?
```

You should see: the Butler answers in the room. TODO(lead): exact first
message to suggest and what the reply looks like.

If nothing comes back: on the computer run `remuda butler status`. It prints
`butler: up (claude)` when the Butler is ready. `launching` means wait. If it
prints `failed`, see step 6.

## 5. Add your second and third machines

Do steps 1 and 2 on machines B and C. The machines must reach each other,
for example over your VPN. Then, on every machine:

```sh
remuda cluster init
```

You should see `Cluster initialized`, then this machine's `Node:` and
`Fingerprint:`.

On A, create one invitation per machine:

```sh
remuda cluster invite
```

You should see:

```text
Invitation for one machine, valid 10 minutes. Run this on the other machine:

  remuda cluster join 'SHA256:...' 'remuda-join-v1 ...'
```

Run that `remuda cluster join` line on B. You should see
`Joined node-... (fingerprint SHA256:...).` with A's fingerprint.

Run `remuda cluster invite` on A again, and run the new join line on C.
Each invitation works for one machine only.

Check membership on any machine:

```sh
remuda cluster nodes
```

You should see three rows in state `admitted`; `*` marks this machine:

```text
 NODE            FINGERPRINT       STATE    VERSION  BY
*node-bwwldaiy   SHA256:BwWl...    admitted 1        SHA256:BwWl...
 node-rvpzy1iz   SHA256:rvPZ...    admitted 1        SHA256:BwWl...
 node-egrdsjir   SHA256:EgrD...    admitted 1        SHA256:BwWl...
```

If a join fails with `Next: check the invitation and try again.`, the
invitation was used or expired: run `remuda cluster invite` on A again.
[cluster-demo.md](cluster-demo.md) also covers remote input and revoke.

## 6. When something goes wrong

The Butler tries Claude first, then Codex. On the computer,
`remuda butler sessions` lists each attempt and why it was skipped, under
`BUTLER ATTEMPTS`.

- **Agent not logged in.** The attempt shows `claude: login (...)` and the
  Butler moves on to Codex. Fix: `claude auth login` (or `codex login`), then
  `remuda butler doctor`. On the phone: TODO(lead): what the Butler says.
- **Stuck prompt or modal.** The Butler answers known startup dialogs itself,
  including the workspace trust question for the folder it launched in. An
  unknown dialog ends that attempt as `claude: dialog (...)` with the screen
  text. Fix: `remuda attach NAME` with the session name from
  `remuda butler sessions`, answer the dialog, then detach with Ctrl-\.
  TODO(lead): what the Butler says on the phone.
- **Weekly limit reached.** TODO(lead): Butler main has no handling for a
  usage limit yet. Say what the user sees and what to do.
