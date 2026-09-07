# Embedded SSH

Starcom uses **sunset-client** for configuration, routing, trust, and identity
handling, with Sunset 0.6 and RustCrypto underneath. `polling` supplies socket readiness without an async
runtime. There is no local SSH subprocess, libssh2, OpenSSL, ring, or AWS-LC in
the selected SSH/crypto path. The remote side remains an ordinary Linux sshd and
stock tmux.

This is still an experimental embedded client, not a claim of OpenSSH-equivalent
configuration coverage or a security audit.

## Connection paths

The GUI discovers supported profiles from `~/.ssh/config`. The diagnostic CLI
accepts explicit options:

```sh
cargo run --locked --bin starcom-inspect -- \
  --host server.example.com --user your-user --session work \
  --known-hosts "$HOME/.ssh/known_hosts" --agent
```

Use `--identity FILE` for an unencrypted OpenSSH-format private key. Encrypted
keys should be unlocked in an SSH agent first. `starcom-inspect --watch N` uses
the same snapshot/live transport but stays an inspection tool; interactive input
is provided by the desktop client.

## SSH-config discovery

The desktop loads the user's `~/.ssh/config` when the workspace starts and on
**Reload config**. Literal `Host` names are shown as suggestions. Wildcard host
blocks still provide defaults but are not listed as destinations.

The embedded resolver currently supports:

- `Host` wildcard/negation matching and first-value-wins ordering;
- `HostName`, `User` (the local account if omitted), `Port`, and `HostKeyAlias`;
- every `IdentityFile` in order, plus `IdentitiesOnly`; if none are set, every
  existing default Starcom can sign (`~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`);
- one `UserKnownHostsFile`;
- bounded `Include` expansion, including `*` and `?` globs;
- `ProxyJump` chains of up to four independently authenticated and verified hops.

Files, total bytes, include depth, directory entries, aliases, and path expansion
are bounded. Reading configuration never executes a command. `%h`, `%n`, `%r`,
`%p`, `%d`, `%%`, and `~/` are handled where supported.

The following are not approximated: `Match`, nested jump routes, `ProxyCommand`, host
certificates, custom agent routing, hardware security keys, algorithm overrides,
canonicalization, binding directives, revoked-key files, and password/MFA
workflows. A profile using unsupported routing, authentication, or trust policy
is shown as blocked. Starcom does not silently connect directly, select a
different key, or ignore those semantics.

An optional system-SSH transport remains a future escape hatch for advanced
enterprise/cluster configurations.

The pinned `sunset-client` drives nested SSH sessions over `direct-tcpip` channels.
Every hop resolves its own configuration and verifies its own host key before
loading identities. Explicit hop user/port overrides apply before path expansion.
The chain shares one wakeable socket and bounded queues. A refused hop never
falls back to a direct connection. Connect, discovery, reconnect, and SFTP upload
all receive the same resolved route; no Starcom-specific jump transport remains.

Sunset 0.6 emits no keyboard-interactive event and has no certificate path, which
is what blocks MFA and host/user certificates rather than any decision here.

Reusing one connection across tabs is a separate question. The fork caps a
connection at sixteen channels, but a shared transport would couple every tab on
a host to one failure. Each tab keeps its own connection.

## Trust

Known-host verification occurs **before** loading an identity file or contacting
the local agent, and it is repeated during rekey.

Supported known-host entries:

- exact host names and comma-separated exact names;
- exact `[host]:port` names for non-default ports;
- OpenSSH `|1|...` hashed host names;
- Ed25519, RSA, and ECDSA P-256 public keys supported by the SSH backend.

`HostKeyAlias` is the name looked up in that file. TCP still connects to
`HostName`; the alias is not a second hop.

Unknown keys and changed keys are distinct failures. Starcom displays the
presented SHA-256 fingerprint and never edits the trust file automatically.

Markers such as `@revoked` and `@cert-authority`, wildcard/negated host policies,
and malformed or unsupported rows currently make the supplied trust file fail
closed. This is intentionally conservative: deleting policy rows to make Starcom
connect is not a supported workaround. Full marker/pattern policy needs its own
implementation or a system-SSH adapter.

## Authentication

Identity files from the profile are offered in order, then keys from the local
SSH agent. `IdentitiesOnly yes` restricts agent offers to configured public
identities, including public halves of encrypted keys. Multiple `IdentityFile`
entries are kept. If the profile names none, Starcom offers every existing
default it can sign (`~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`) before the
agent. Encrypted files are skipped with a note to `ssh-add` them. There is no
agent-versus-key radio on the form; an extra identity path under Advanced is
tried first.

Hardware-backed `sk-ssh-ed25519@openssh.com` keys in the local agent are
offered; the authenticator signs, Starcom does not talk to the FIDO device.
`sk-ecdsa-*` is still listed as skipped. Direct-file SK signing would need
libfido2, which Starcom does not link.

If no agent is reachable and no identity files exist, the connection form says
so before you press Connect, and the failure names the socket it tried and what
to do about it. A desktop session frequently does not inherit `SSH_AUTH_SOCK`
from a shell, so an agent that works in a terminal can be invisible to a
launcher-started Starcom. An agent that is running but holds no keys is
reported as such rather than as a generic authentication failure.

Supported identity sources:

- unencrypted OpenSSH Ed25519 private keys;
- RSA keys using `rsa-sha2-256` signatures;
- ECDSA P-256 keys;
- a local OpenSSH agent (Ed25519, RSA, ECDSA P-256, and `sk-ssh-ed25519`).

On Unix, the agent is selected through `SSH_AUTH_SOCK`. On Windows, Starcom uses
the local OpenSSH-agent named pipe, or another local named pipe specified through
`SSH_AUTH_SOCK`. Pageant and Unix-socket compatibility bridges on Windows are not
implemented.

Private-key file bytes are kept in zeroizing storage. Identity and agent handles
are released after authentication. Agent messages, key counts, field lengths,
and signature algorithms are bounded and validated. Starcom does not forward the
agent and does not fall back to passwords.

## Transport behavior

A desktop worker owns one nonblocking TCP connection and one non-PTY exec channel.
The GUI lock is never held across DNS, file, agent, SSH, tmux, or snapshot I/O.

The transport:

- preserves stdout and stderr as separate streams;
- bounds queued stdout to 1 MiB and stderr to 64 KiB;
- stops reading under backpressure rather than dropping terminal bytes;
- handles partial socket writes without replaying accepted bytes;
- applies per-operation deadlines to network and agent I/O;
- validates the host key on each key exchange;
- supports rekeyed streams in the integration fixture.

OS DNS resolution, filesystem access, and initial agent connection may block
outside the network timer, but they run on a worker rather than the window thread.
Cancellation invalidates publication immediately and wakes active socket waits.
A worker stuck in an uncancellable OS call is detached during shutdown rather than
freezing window closure; stale results cannot enter a stopped/new connection.

## Reconnection

A dropped attachment is classified before anything is retried. Only transport
loss — a reset socket, an operation deadline, or a control channel that ended
without tmux announcing an exit — is retried automatically. Authentication and
host-key failures are never retried, so a wrong key or an untrusted host cannot
become an endless security retry. A missing or destroyed session, a tmux server
that exited, and an explicit detach also stop, because reattaching by name after
any of those can land on a different session.

Classification prefers typed transport errors over remote text. Where tmux's own
stderr is consulted, it may only *narrow* a failure to a non-retriable one; it can
never promote a failure to retriable. Remote output is untrusted, so it is allowed
to stop Starcom from retrying but never to make it retry.

Retries use exponential backoff from 500 ms to a 30 s ceiling with per-connection
jitter, and the wait is cancellable at any point. Each attempt takes a fresh
connection epoch, which invalidates every outstanding input token, and rebuilds
all pane models from a new snapshot boundary. Nothing typed while disconnected is
queued, and no write with uncertain delivery is repeated.

Sunset currently has a small receive window and a narrower algorithm set than
OpenSSH. High-latency throughput and compatibility with additional servers need
measurement before broad support claims.

## tmux control attachment

The SSH exec channel runs stock tmux without a PTY:

```text
tmux -N -C ... attach-session -E ...
```

`-N` prevents creating a replacement tmux server. `-E` avoids changing the
session environment. Read-only attachments use `ignore-size` so merely opening
does not change the shared layout. Interactive attachments omit `ignore-size`
and report their cell size with `refresh-client -C`, using the same font metrics
as painting, so pane column counts match the GUI.

If public-key authentication runs out of identities, the error distinguishes
"nothing supported was available to offer" from "the server rejected the keys
that were offered", and names skipped files and unsupported agent algorithms.
An agent that lists only unsupported key types fails at load rather than after
a confusing empty offer.

TCP keepalive is enabled so an idle control session is not black-holed by NAT.
On Linux and macOS the first probe is after 30 seconds of silence, then every
5 seconds; four unanswered probes close the socket. That is not a tmux ping;
the desktop's displayed round-trip is the last small control command we already
sent. A tmux write or reply deadline is a timeout (transport loss), not a
protocol fault: a sleeping laptop or a black-holed NAT mapping must reconnect
rather than sit on "Connected" until the next keystroke fails closed.

Read-only connections add tmux's `read-only` client flag. Interactive connections
attach without that flag, but Starcom's application gate stays closed until the
initial snapshot, client metadata, aliases, and relevant hooks pass validation.

The tmux read-only flag is not an authorization sandbox. Starcom constructs a
restricted command set from typed IDs and validated values. It refuses command
aliases or hooks that would change the meaning/ordering of snapshot or interactive
transactions.

## Interactive safety

Terminal text bytes are hex-encoded for `send-keys -H`. Named special keys are
selected from a fixed enum. Clipboard text is uploaded through a private,
collision-resistant named tmux buffer using bounded octal-encoded chunks; it does
not overwrite the user's default buffers or remote clipboard.

Every action carries the exact connection epoch, reconstructed-view generation,
pane ID, session ID, window ID, pane geometry, and position observed when it was
created. tmux evaluates a server-side guard immediately before the action. Input
is blocked when the pane/session/layout changed, the pane is dead/in a mode/input-
disabled, or `synchronize-panes` is enabled. Resize is additionally blocked for a
zoomed window.

A blocked action is not retried. A transport/command failure is reported as
possibly uncertain and tears down the interactive stream. Starcom never queues
input while offline and never replays uncertain keystrokes after reconnect.

Remote resize is explicit per connection. On a successful resize request, the
old view is invalidated and reconstructed from tmux's resulting layout before
more input is accepted.

## Snapshot and live output

The control attachment starts with pane output paused for that client, discovers
and validates topology, captures observable primary/alternate state and bounded
history, establishes a final reply boundary, then enables live output. Fresh
Alacritty models are published only after the transaction completes.

Layout changes and unknown mutations invalidate the view. Reconstruction replaces
models instead of appending a capture to stale history. See
[SYNCHRONIZATION.md](SYNCHRONIZATION.md) for the exact ordering argument and the
terminal state that stock tmux does not export.

## Build policy

```sh
python scripts/check-dependencies.py
```

checks the resolved host dependency graph for forbidden native SSH/crypto
backends. CI also sets unusable `CC` and `CXX` values so an accidental native
source build fails. This does not prohibit normal platform linking, graphics
drivers, OS FFI, or Rust build-helper crates that do not invoke a C compiler under
the selected features.

The isolated Linux fixture installs `sshd`, `ssh-keygen`, `ssh-agent`, `ssh-add`,
and tmux as **test/runtime programs**. They are not linked into Starcom. The
fixture creates temporary keys, a loopback-only sshd, and a unique tmux socket,
then removes them without touching the user's server or default tmux socket.

Current automated coverage includes:

- unknown/changed/revoked trust failures before authentication;
- exact and hashed known-host entries;
- Ed25519/RSA/P-256 identity authentication and local-agent signing;
- 128 KiB streams across forced rekeys;
- separate stderr, deadlines, snapshot continuity, and hook rejection;
- real desktop-worker input and paste arriving exactly once;
- remote pane resize followed by server-authoritative resynchronization;
- `synchronize-panes` blocking targeted input;
- preservation of remote processes, pane/session state, and environment.

Native Windows agent signing runs in Windows CI. Broader server algorithms,
certificates, bastions, MFA, macOS agent variants, hardware keys, and high-latency
throughput remain open.
