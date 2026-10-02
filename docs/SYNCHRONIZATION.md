# Snapshot-to-live synchronization

This describes the snapshot-to-live path shared by the desktop client and
`starcom-inspect --watch N`. It reconstructs observable pane state and then
consumes `%output` notifications. It is not a complete serialization of tmux's
terminal parser. `Watching` is a stream state, not a claim of perfect fidelity.

Every attachment runs this path from the beginning, including each automatic
reconnection: models are rebuilt from a fresh boundary under a new epoch and are
never appended to the models of a lost attachment.

## Capture boundary

1. Attach using the existing verified SSH transport and stock `tmux -N -C`.
   Keep `attach-session -E` with `no-output`. Read-only adds `read-only,ignore-size`;
   interactive omits `ignore-size` and later reports `refresh-client -C`.
2. Set `no-output` on this client and consume the reply, draining earlier output.
3. Discover pane IDs. Reject aliases overriding snapshot commands and configured
   after-capture/display/list/refresh hooks at global, session, window, and pane
   scopes. Send one newline-terminated command list containing, for
   each pane, state metadata, joined current screen/history, saved primary
   screen/history, and pending parser bytes. End with a fresh state table,
   session identity, and `refresh-client -f '!no-output'`.
4. Correlate each command reply. Validate IDs, dimensions, modes, budgets, and the
   final state table before allocating replacement models. A missing pane,
   command error, inconsistent state, or incomplete transaction fails the restore.
5. Construct new models. Only notifications after the final enable-output reply
   are applied. Output in that same SSH read is retained; a read boundary is not
   a synchronization boundary. Publish the new view only after reconstruction.

The design relies on tmux executing this list of synchronous commands without
returning to its event loop, and on its ordered control-output queue. The tmux
source explicitly implements both behaviors; this was read in 3.3a and the
behavior is exercised against the 3.4 build the integration fixture uses. This is a source-supported design,
not a new atomic-snapshot guarantee in tmux's public API. Runtime ordering tests
must pass on every advertised tmux version before it is called supported.

A yielding `after-capture-pane` hook can break the uninterrupted batch assumption.
Preflight refuses configured snapshot-command hooks and overriding aliases without
removing or executing those capture hooks. This check is not a configuration lock:
a different client changing hooks after preflight is outside the initial target.
The metadata comparison catches some changes, not arbitrary content changes with
identical metadata. Do not claim exact synchronization for concurrent configuration
mutation or unsupported hook behavior.

## What is restored

The primary and active alternate screens, bounded scrollback, visible cell
attributes, cursor position (including pending wrap), alternate-screen saved
cursor, scroll region, tab stops, exported keyboard/mouse modes, and pending
terminal bytes are reconstructed. tmux does not resize the saved primary-screen
cursor when a pane on the alternate screen shrinks, so those coordinates are
clamped to the current pane rather than failing the snapshot. `capture-pane -J` preserves soft-wrap joining.
When tmux does not export `bracket_paste_flag` (as in the tested 3.4 build), its
value remains explicitly unknown in the snapshot metadata rather than guessed.

Capture escaping needs its own decoder: screen text uses doubled backslashes,
while synthesized controls and pending bytes use octal escapes. Decoding is done
once, preserving a literal backslash followed by `033` as text.

After capture, live bytes are decoded by tmuxctl and fed directly to the pane's
Alacritty model. Device-query and clipboard side effects remain suppressed;
tmux, not this frontend, owns the application-side terminal connection.

## Fidelity limits

Stock tmux does not expose every detail needed for an exact Alacritty checkpoint.
This implementation does not recover the current SGR drawing pen, arbitrary
saved DEC cursor state, the complete charset/parser state, origin mode, palette
changes, hyperlinks, terminal graphics, or all extended keyboard modes. It resets
the unobservable drawing pen rather than pretending the last captured cell is the
current pen. Later output can therefore differ until the application resets or
redraws the relevant state. Initial cell attributes are distinct from that pen.

A full-screen program's conversation history is not necessarily terminal
scrollback. History is bounded locally and remotely. Alternate-screen panes
and a History setting smaller than tmux's buffer are not reported as faults.

The strict control parser still rejects capture text that looks exactly like a
control reply guard. That inherited framing limitation must not be mistaken for
support for every arbitrary transcript.

## Invalidation and limits

Layout/window changes, unknown pane output, pauses, session changes, and unknown
notifications invalidate the view. Further bytes are not fed into invalidated
models. `synchronize()` rebuilds from a new boundary; it does not append a fresh
capture to the old terminal. The watch CLI resynchronizes when needed and reports
that continuity was lost. Old views remain readable on failure or disconnect.

There is no pane-input API on this path, no offline keystroke queue, and no
network reconnect scheduler. It does not promote the existing connection policy
into its interactive `Live` state.

The initial limits are 32 panes, 1,000 requested history rows per pane, a 64 KiB
outgoing command list, 256 pending commands, 8 MiB per batch, the existing 1 MiB
reply budget, and a conservative 1,048,576-cell aggregate allocation budget.
Budgets are explicit safety limits, not measured performance claims.

## Validation gate

The change includes network-free tests for capture escaping, pending sequences,
alternate-screen return, wrap state, custom tabs/margins, allocation limits, view
invalidation, and output sharing a packet with the last reply. Linux live tests
cover pre-existing primary/alternate screens, pending-byte continuation, resize
resynchronization, detach/job preservation, continuous output across the boundary,
refusal of yielding user hooks, and the `--watch` CLI.

Local validation used Rust 1.98.0 and tmux 3.4 on Linux: all 46 network-free tests,
36 no-SSH core tests, Clippy with warnings denied, rustfmt, and 11 live SSH/tmux
tests passed. The live continuity test verifies all 200 records, including
scrollback, without gaps or duplicates across attachment. The pre-existing
agent-authentication test was not rerun because this environment lacks an
OpenSSH agent executable; all five new live tests ran. The fixture used a
separate loopback sshd and isolated tmux socket, not a user's server.

Windows/macOS validation of these changes, wider tmux-version coverage, and
native TUI/Codex acceptance remain open. Reproduce the full CI fixture with:

```sh
cargo fmt
cargo fmt --check
cargo test --locked --all-targets
cargo test --locked --no-default-features --all-targets
cargo clippy --locked --all-targets -- -D warnings
scripts/test-ssh.sh
```

The last command uses its own disposable Linux SSH/tmux fixture. Do not run
integration experiments against an existing production tmux server. Native
Windows/macOS/Linux GUI, clipboard, IME, and Codex acceptance are later gates.

## Source references

- [tmux control-mode protocol](https://github.com/tmux/tmux/wiki/Control-Mode)
- [Ordered output and pane offsets, tmux 3.3a](https://github.com/tmux/tmux/blob/3.3a/control.c)
- [Synchronous command queue, tmux 3.3a](https://github.com/tmux/tmux/blob/3.3a/cmd-queue.c)
- [Capture escaping, tmux 3.3a](https://github.com/tmux/tmux/blob/3.3a/grid.c)
- [iTerm2 state fields](https://github.com/gnachman/iTerm2/blob/master/sources/tmux/TmuxStateParser.m)
- [Alacritty terminal 0.26 API](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/term/struct.Term.html)
