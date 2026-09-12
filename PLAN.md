# Starcom plan and roadmap

Updated: 2026-09-07.

**Current status:** M0 through M3 are done for the tested configuration, and M4
is partly done. M2's gate is signed off; M3 is complete: transport loss is classified apart
from authentication, trust, missing-session, server-exit, and detach; retries use
cancellable jittered backoff with visible state; every attempt takes a fresh
epoch and rebuilds models; a replaced session or restarted tmux server is
reported. Alternate-screen panes and a History setting below tmux's buffer are
not treated as lost output. Loss during output, input, paste, and a remote layout
change, plus a restarted tmux server, are covered by fixture tests.

M4 now persists non-secret tabs, resumes their saved sessions by default with an
opt-out setting, and adds explicit session discovery and creation. ProxyJump
uses Sunset's shared std-only client. Connection reuse and certificate/MFA
workflows remain open. M5 is next.

Development lands on protected `main` through short-lived pull requests. CI runs
for pull requests and updates to `main`. Routine milestone branches, generated
recovery trees, and shadow copies of the source are not part of the workflow;
the application source lives in the normal repository root.

## Product goal

Starcom is a small, fast, cross-platform GUI for persistent remote terminal
sessions. It replaces tmux's terminal UI—not tmux itself—with normal tabs, mouse
selection, scrolling, copying, and draggable pane dividers.

The client targets Linux, macOS, and Windows. The remote side initially targets
Linux with stock OpenSSH and stock tmux. A user must be able to close Starcom or
fall back to an ordinary tmux client without affecting remote jobs.

Priorities, in order:

1. Never lose, duplicate, or silently redirect user input.
2. Recover a truthful terminal view after connection loss.
3. Require no Starcom daemon, patched tmux, extra port, or privileged install.
4. Keep startup, idle CPU, memory, dependencies, and UI complexity modest.
5. Add features only after the core shell/Codex workflow is dependable.

## Architecture

```text
Starcom client (Linux / macOS / Windows)
  winit event loop + Blade renderer
    egui connection tabs, forms, window tabs, dividers and dialogs
      one Alacritty terminal model per tmux pane
        snapshot/live coordinator and guarded input queue
          bounded tmux control-mode adapter (tmuxctl)
            embedded Sunset/RustCrypto SSH transport
                       |
                    encrypted SSH
                       |
Linux host: stock sshd -> stock tmux -C -> existing tmux server -> PTYs/apps
```

One registered Starcom tab owns one client worker and one tmux-session view. The
`+` composer owns the only connection form; pressing Connect promotes it to a
registered tab while the attachment starts. A failed first attachment moves the
form back to `+` for repair instead of leaving an empty tab. Panes are never
mixed with another tab's connection state.

The SSH channel is opened without a PTY and executes `tmux -N -C attach-session`
against an existing session. `-N` prevents accidental server creation. Control
mode provides pane output, topology notifications, and commands. A single tmux
session attachment carries all of its panes; there is no SSH connection per pane.

### Why not tmux's native socket protocol?

The native tmux client/server protocol is local Unix IPC and transfers operating-
system file descriptors. It is not a remote, cross-platform protocol. Bridging it
would require a custom Linux-side component and would recreate the deployment and
compatibility burden Starcom is intended to avoid. Documented `tmux -C` control
mode is the external frontend interface and works with the host's installed tmux.

## Decisions and invariants

| Area | Decision |
| --- | --- |
| Remote lifetime | Existing tmux server owns jobs and PTYs. |
| Remote deployment | Stock sshd and tmux only; no Starcom service. |
| GUI | Blade + blade-egui + egui + winit, following FileMan conventions. |
| Terminal model | `alacritty_terminal`; do not implement a VT emulator. |
| Control protocol | `tmuxctl` with runtime/spawn defaults disabled. |
| SSH | Embedded Sunset + RustCrypto; no libssh2/OpenSSL/ring/AWS-LC backend. |
| Dependency risk | Owning the SSH stack means owning its advisories: `deny.toml` gates every change and runs weekly. |
| Automatic retry | Transport loss only. Authentication, trust, missing-session, server-exit, and detach always stop. |
| Advanced SSH escape hatch | Optional system-SSH transport remains a future option, alongside the Sunset fork. |
| Third-party patches | Forks pinned by revision on `main` (Blade, Sunset). Never vendored, never unpinned. |
| Saved state | Destinations, preferences, and last-used session name. Startup reconnects saved tabs using current SSH policy unless disabled. An unreadable file is a system dialog: clear it, or exit and leave it. |
| Session creation | Only from an explicit action. No failure path may create a session or a server. |
| Repository | One Rust 2024 package; normal root sources on `main`. |

The following are correctness rules, not optional polish:

- Attach to an existing session. Never silently create a replacement session or
  tmux server after a failure.
- Verify the host key before loading a private key or contacting an agent. Unknown
  and changed keys block the connection.
- Bound config files, protocol lines, command replies, terminal allocations,
  stderr, pending actions, paste size, and network work per iteration.
- Every connection and reconstructed view has an epoch/generation. Late worker
  results and stale UI actions cannot target a newer session.
- Input is enabled only after a coherent snapshot is published. Nothing typed
  while disconnected is queued for later replay.
- A write with uncertain delivery is never retried automatically.
- Only transport loss is retried automatically, and remote text may narrow a
  failure to non-retriable but never promote one to retriable.
- Paste is sent immediately. Paste content is data, never concatenated into tmux
  command syntax.
- Targeted input is blocked when `synchronize-panes` could broadcast it. Starcom
  does not change that user's tmux option.
- Remote resizing is opt-in because it changes the shared tmux layout. The server's
  resulting geometry is authoritative and triggers reconstruction.
- Unsupported SSH routing, trust, or authentication directives are shown as
  blockers; Starcom does not quietly connect with different semantics.
- Persisted state is destinations and preferences, never credentials, host-key
  material, or terminal contents. Restoring a workspace reconnects saved tabs
  to their existing sessions by default, validating saved pane/window hints
  against each fresh snapshot before using them.
- Session discovery uses `tmux -N` and cannot start a server. Starcom asks on its
  own only after an attach failed because the session was missing, when the user
  has already authenticated and the list is the answer. Creating a session is
  reachable only from a confirmed action, never from a failed attach.
- Closing a tab or the application detaches its client while leaving remote jobs
  alive. Graphics resources are released while the native event loop/display is
  still valid.

## Current implementation

### Session and terminal core

- Bounded tmux-control parsing and typed command construction.
- Fresh Alacritty models reconstructed from observable primary/alternate screens,
  bounded history, cursor, exported modes, tabs, margins, and pending bytes.
- A tested snapshot/live ordering boundary; live bytes in the same SSH packet as
  the final reply are retained.
- Model invalidation and full reconstruction after layout/session changes.
- Last disconnected view remains readable and copyable.

Stock tmux does not expose all terminal parser and drawing-pen state. These limits
remain documented in `docs/SYNCHRONIZATION.md`; Starcom must not claim a universal
terminal checkpoint.

### SSH and configuration

- Strict known-host verification with exact, port-qualified, and hashed entries.
- Ed25519, RSA/SHA-256, and ECDSA P-256 identity files; Unix and Windows OpenSSH
  agents; no password or MFA fallback yet.
- GUI discovery of literal `Host` aliases from `~/.ssh/config`, including bounded
  `Include` expansion and wildcard defaults.
- Supported resolution: `Host`, `HostName`, `User`, `Port`, `HostKeyAlias`, every `IdentityFile`
  in order, `IdentitiesOnly`, one `UserKnownHostsFile`, and OpenSSH's default
  `id_ed25519` / `id_ecdsa` / `id_rsa` when `IdentityFile` is omitted. Files
  are offered first, then the agent; `IdentitiesOnly` restricts its offered keys.
- Flat ProxyJump chains use `sunset-client`; every hop has independent trust and
  identity policy. `Match`, nested routes, ProxyCommand, certificates, custom agents,
  and security-algorithm overrides are reported as unsupported rather than ignored.

### Desktop

- Independent session tabs; `+` owns the only connection form, and a tab is
  registered on Connect. Failed first attachments return to `+`.
- Host-first connection: known `Host` aliases, automatic session listing, first
  session selected, Connect without a separate list step.
- One Starcom tab is one tmux session and shows one window of that session.
  A window picker is deferred.
- Interactive clients report `refresh-client -C` from GUI font metrics so
  tmux cell counts match the painted pane; divider drags send `resize-pane`.
- Local scrollback, selection, word/line selection, copying, and font sizing.
- Interactive UTF-8/IME commits, conventional special keys, terminal Ctrl keys,
  and clipboard paste. Unmodified clicks are forwarded when the pane asked for
  mouse reports; drags stay local selection. File drops upload over SFTP.
- Divider dragging that resizes tmux on release.
- Event-driven redraw and a worker thread that does no network I/O under the UI
  model lock. Hidden-tab output updates its activity timer without repainting an
  unchanged selected terminal.
- Automatic reconnection after transport loss: classification, cancellable
  jittered backoff, a visible attempt/countdown with a stop control, a fresh
  epoch and rebuilt models per attempt, and reports when the reattached session
  or its scrollback is not continuous with what was on screen.
- Saved connection tabs reattach to their previous host/session automatically by
  default, using normal host-key and authentication policy, with an opt-out
  setting in About.
- Explicit session listing (`tmux -N`, which cannot start a server) and confirmed
  session creation, which is the one path allowed to start one.
- Ordered shutdown and a native Linux/X11 close-path test.

## Milestones

### M0 — Reusable foundation: complete

- [x] One-package Rust layout, FileMan-style organization, and cross-platform CI.
- [x] Validated IDs/dimensions and safe tmux command construction.
- [x] I/O-independent `tmuxctl` adapter with bounded framing and reply lifecycle.
- [x] Connection epochs and no-offline-input state machine.
- [x] Per-pane Alacritty models and deterministic replay fixtures.

### M1 — Existing-session attachment: implemented, compatibility work remains

- [x] Rust-native embedded SSH, host-key verification, identity and agent auth.
- [x] Non-PTY control attachment to an existing tmux session only.
- [x] Topology discovery and observable snapshot-to-live reconstruction.
- [x] Isolated Linux sshd/tmux integration tests with operation deadlines.
- [ ] Test additional tmux versions and distributions.
- [ ] Exercise editors, monitoring TUIs, and Codex; catalog fidelity gaps.
- [ ] Decide which remaining unexported parser states require mitigation.

### M2 — Minimal interactive desktop: implemented, acceptance gate still open

- [x] Blade/egui/winit desktop with event-driven redraw.
- [x] Connection tabs whose form and terminal workspace replace one another.
- [x] SSH-config alias suggestions and fail-closed profile resolution.
- [x] Local scrollback, selection, copy, and pane layouts.
- [x] Wheel to the application when it reports mouse or is on the alternate screen;
  pane split/zoom/kill/move chrome; session rename; oversize drop confirm.
- [x] Guarded keyboard input and paste; no stale/offline replay.
- [x] Remote divider resizing followed by server-authoritative resync.
- [x] Native Linux/X11 render, clipboard, resize, and clean-close smoke test.
- [x] Real localhost SSH/tmux test for input, paste-once, resize, and broadcast guard.
- [ ] Native macOS and Windows GUI interaction/close tests.
- [ ] Wayland, IME, high-DPI, application mouse drags, and real Codex acceptance.

Gate: normal shell and Codex work should be comfortable without tmux keybindings,
and closing/reopening the client must not affect jobs or misdirect input. Signed
off by the maintainer on 2026-08-30 on the strength of the automated Linux/X11
coverage and hands-on use. The two unchecked items above stay open as work, not
as a gate: they are acceptance breadth, and the milestone no longer waits on them.

### M3 — Trustworthy automatic reconnection: complete for the tested configuration

- [x] Classify transient transport loss separately from auth, trust, missing-session,
  tmux-restart, and explicit user detach.
- [x] Cancellable exponential backoff with jitter and visible retry state.
- [x] Reattach and reconstruct fresh models for every new epoch.
- [x] Preserve the last view but never queue offline input or replay uncertain writes.
- [x] Test connection loss during output, input, paste, and remote layout changes.
- [x] Treat a tmux write/reply deadline and a machine suspend (wall time running
  ahead of the monotonic clock) as transport loss, so a sleeping laptop
  reconnects instead of failing as a protocol fault. TCP keepalive probes every
  5s after 30s idle, four misses.
- [x] Report a replaced session or restarted tmux server. History is bounded;
  alternate-screen panes and a History setting below tmux's buffer are not
  warned as discarded output.
- [x] Reattach across a genuinely restarted tmux server and report it as a
  replacement. A fresh server numbers its first session `$0` again, so identity
  is the server pid, session id, and session creation time together.
- [ ] Exercise loss on a real network path, not only a killed local control
  client: latency, a suspended laptop, and a NAT rebind.

Gate: repeated network loss produces no duplicate command, hidden gap, stale-pane
write, endless security retry, or phantom successful reconnection. Met for the
tested configuration; each clause has a fixture test, and the reconnect test is
verified to fail when automatic retry is disabled.

### M4 — Multiple-machine operational fit: three items done

- [x] Persist non-secret tabs/profiles and resume their saved sessions by
  default, with an opt-out setting.
- [x] Add explicit session discovery/creation UI without changing attach semantics.
  Listing is also offered on its own after an attach fails because the session is
  missing: the user has already authenticated and the list is the answer. No
  other failure asks, and listing still never attaches or creates.
- [ ] Reuse one SSH connection per host where the SSH backend supports it safely.
  The fork caps a connection at `config::MAX_CHANNELS = 16`, so the sixteen tabs
  could share one from a channel-count perspective. Sharing still couples every
  tab on a host to one transport, and that is why it was deferred. The channel
  count was never the blocker.
- [x] Add ProxyJump/bastion support through the shared `sunset-client` package.
  Up to four flat hops use independent configuration, identity, and host-key checks.
  There is no direct fallback. Nested routes and ProxyCommand remain unsupported.
- [ ] Improve certificate, hardware-key, custom-agent, and MFA workflows.
  Sunset 0.6's client emits no keyboard-interactive event and has no certificate
  path, so MFA and certificates cannot be driven from it at all. Agent-held
  `sk-ssh-ed25519@openssh.com` keys are offered; signing stays in the agent
  (no libfido2). `sk-ecdsa-*` is still skipped as unsupported. Direct-file SK
  and custom `IdentityAgent` sockets remain blockers.

Gate: a user with several hosts can reopen yesterday's tabs, see what is running,
and start what is missing, without Starcom authenticating or creating anything on
its own. Met for persistence and discovery. The remaining three items are a
milestone of their own; the forwarding blocker among them is now carried as a
pinned fork, leaving the transport work rather than an open question.

### M5 — Performance and release hardening

- [ ] Measure startup, idle CPU, RAM per pane/history, sustained-output throughput,
  and input latency. First baselines, x86-64 Linux, thin LTO: a clean release
  build takes about 3 minutes and produces a 14.4 MiB stripped binary.
- [ ] Stress many panes, long lines, slow readers, reconnects, and repeated resizes.
- [ ] Fuzz hostile control/config transcripts and audit terminal side effects.
- [ ] Publish a tested tmux/platform compatibility matrix.
- [ ] Leave every release-candidate crypto dependency behind. `ssh-key` and the
  `rsa` crate it pulls in are both pre-release, and `rsa` carries RUSTSEC-2023-0071
  with no fixed version published. Either reach a fixed release or ship without
  the `rsa` feature; `deny.toml` records the accepted exposure until then.
- [ ] Replace the pinned `blade` git revision with published crates. Git
  dependencies cannot be published to crates.io, so this blocks crates.io
  packaging. GitHub Release binaries are built from the git pin.
- [ ] Replace Sunset SFTP's unmaintained `paste` proc macro with maintained
  identifier generation. It runs only at compile time and is temporarily
  accepted in `deny.toml`; remove that exception with the dependency.
- [x] Unsigned GitHub Releases from `v*` tags: Linux tar.gz, AppImage, deb, and
  rpm; macOS aarch64 zip and DMG (ad-hoc codesign); Windows zip. App icon,
  desktop entry, and window/PE resources are included.
- [ ] Package signed Linux, macOS, and Windows builds.

## Rust-only HTTPS evaluation

`tls-evaluation` exposes a read-only `tls-probe` example using Fileman's shared
`navigato-http`. It is off by default and never enables reporting or updating.

```sh
cargo run --locked --no-default-features --features tls-evaluation --example tls-probe -- https://api.github.com/
```

The pinned RustCrypto provider is experimental. Its adapter rejects low-order
X25519 and compressed P-256 shares, uses fallible key generation and limits
AES-GCM key usage. Shared tests own cipher/certificate/transport coverage;
Starcom CI checks feature composition on each OS and the MSRV. No forked
primitives or TLS engine. Root-store FFI is permitted only through
`rustls-native-certs`, not native TLS.

Release blockers: upstream provider review, independent interoperability/fuzzing,
proxy/cancellation policy, and direct-reporting consent tests. Existing advisory
exceptions are unchanged; the HTTPS client never loads client private keys.

## Immediate work order

1. Keep the shared client and both application adapters covered by live fixtures.
2. Begin M5 with the measurement harness, not with optimizations. There is still
   no recorded startup time, idle CPU, per-pane memory, or input latency.
3. Add native macOS/Windows close, clipboard, and input acceptance.
4. Exercise reconnection on a real network path, not only a killed local client.
5. Extend SSH configuration only where semantics can be tested end to end.

## Validation policy

Every update must keep these paths green:

```sh
cargo fmt --check
python scripts/check-dependencies.py
cargo deny --all-features check
cargo build --locked --all-features --all-targets
cargo test --locked --all-features --all-targets
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --no-default-features --all-targets
cargo test --locked --no-default-features --features ssh --all-targets
```

CI also builds and tests at the declared `rust-version`, so that number is a
tested claim rather than a guess, and runs the dependency-advisory gate weekly as
well as per change.

Linux CI additionally creates a disposable loopback sshd and isolated tmux socket.
It must never touch the user's default tmux server. The fixture asserts how many
tests actually ran, because `cargo test` exits successfully when a filter matches
nothing and would otherwise report a green run that asserted nothing. Native Linux/X11 automation
covers rendering, system clipboard, window resize, and normal WM close. Windows
CI exercises the native OpenSSH-agent named pipe. Compilation is not considered a
substitute for native interaction testing on platforms where that test is absent.

## Non-goals for the first useful release

File management beyond bounded drop uploads, Git UI, agent orchestration,
plugins, collaboration, terminal graphics protocols, a custom multiplexer, and
a custom remote daemon are out of scope. A full-screen program's conversation
history is not necessarily terminal scrollback, and Starcom cannot reconstruct
output tmux has discarded.

## References

- [tmux control mode](https://github.com/tmux/tmux/wiki/Control-Mode)
- [tmux native client](https://github.com/tmux/tmux/blob/master/client.c)
- [tmux control implementation](https://github.com/tmux/tmux/blob/master/control.c)
- [`tmuxctl`](https://docs.rs/tmuxctl/latest/tmuxctl/)
- [`alacritty_terminal`](https://docs.rs/alacritty_terminal/latest/alacritty_terminal/)
- [Sunset](https://github.com/navigato-rs/sunset)
- [OpenSSH configuration](https://man.openbsd.org/ssh_config)
- [FileMan](https://github.com/kvark/fileman)
