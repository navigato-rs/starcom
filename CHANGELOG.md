# Changelog

## Unreleased

- Creating a session refuses a name that already exists on that server. Each
  tab stays bound to its tmux window id, including when another window shares
  the name.
- A live server whose named window is gone paints only that tab red and offers
  Recreate. The shared attachment stays up; sibling sessions stay green.
- Automatic transport recovery now pauses after ten retries instead of looping
  forever. The frozen tabs remain available with an explicit Reconnect action;
  when a reachable host reports that tmux exited during a reboot, each remembered
  logical session offers Recreate under its saved window name.
- The terminal-cell safety ceiling is enforced per logical tmux window rather
  than across every window sharing a server connection. Unrelated sessions no
  longer exhaust one another's budget; pane count, individual dimensions,
  history, captures, and each window's split tree remain bounded.

## v0.3.1

Shared server workspaces, window-based sessions, a vertical session sidebar,
faster terminal rendering, and more responsive input after `v0.3.0`.

### Sessions and navigation

- Starcom now keeps one SSH connection and tmux control client per resolved
  server. Each user-visible session is a named window in that server's managed
  `starcom` tmux session, so many projects no longer create many SSH clients or
  tmux servers. Saved workspaces reopen those windows through the shared server
  connection.
- The resizable vertical sidebar groups sessions under full-width server
  headers, making ownership visible without taking terminal height. Servers can
  be reordered by dragging; the selected session is not forced back into view
  after deliberate scrolling, and the old 16-session limit is gone.
- Connected sessions and servers are green. The host picker includes live
  destinations absent from SSH config, refreshes discovery for a typed host,
  and distinguishes managed windows from yellow non-managed tmux sessions.
  Those sessions can be explicitly migrated into the managed session, moving
  their windows and entering the first one.
- New session, Settings, Notes, refresh/recovery, and Leave controls fit into two
  compact sidebar rows instead of covering terminal contents. Server headers
  normally show latency, replace it with blue input-backlog bars when useful,
  and show a red `WAIT` when an acknowledgment exceeds
  `max(5 × last RTT, 50 ms)`.

### Session management and recovery

- A pane can move into a collision-free new session on the same server. The new
  session opens immediately with its generated name selected for replacement;
  the action is hidden when it would move a session's only pane.
- Rename now stays on the shared control connection, queues correctly behind a
  pane move, and is persisted only after tmux accepts it. Lost rename replies
  reconnect toward the optimistically saved name without sending the command
  twice, and successful replies no longer deadlock the UI mutex.
- Closing the final pane removes its logical session immediately. Other failures
  keep a red saved session until Leave: a reachable server with a missing window
  offers Recreate, while an unreachable server offers Reconnect. Creating the
  first window enters it even if host discovery has not finished.
- Tmux metadata now uses printable quoted names and escape-aware delimiters for
  compatibility with tmux 3.2. Large escaped output notifications have a 4 MiB
  safety bound, avoiding disconnects during large synchronized TUI redraws.

### Responsiveness and terminal UX

- Ordinary key input uses a bounded pipeline of guarded tmux transactions rather
  than waiting for every earlier acknowledgment. Paste, resize, layout, pane
  move, and rename remain ordering barriers, and uncertain input is still never
  retried. Latency hover text breaks slow input down into local queueing, tmux
  acknowledgment, output, and flow-control time.
- Terminal cells now use one small Blade batch per pane rather than many egui
  text meshes and draw calls. In the demo workspace this reduced tessellation
  from 175 primitives and 6,668 vertices to 10 primitives and 972 vertices, and
  the measured UI-plus-tessellation time from about 235 µs to 190 µs per frame
  on x86-64 Linux in a release build.
- Pane chrome no longer polls or displays working directories, while splits
  still inherit the correct source pane directory. Layout reconstruction
  preserves deliberate history position without carrying stale pixel momentum,
  and successful resynchronization clears its warning.
- Per-window Notes provide bounded key/value metadata backed by tmux user
  options. Editing, insertion, and periodic refresh preserve focus, and terminal
  clicks can safely finish a note edit without dropping the same frame's input.
- Hard-wrapped and indented web links copy as one URL, including links split
  immediately after `https://`. Transient copy and status messages remain in the
  terminal area's overlay line instead of moving content or crossing the
  sidebar.

## v0.3.0

Working-folder chrome, mouse and wheel, and reconnect after `v0.2.0`.

- **Working folder.** The selected pane's top bar shows
  `#{pane_current_path}` on the left and split/move/zoom/close on the
  right. Shown on the primary screen while mouse reporting is off.
  Hover for the full path; it updates after `cd`. Splits start in that
  directory.
- **Wheel and mouse.** The wheel sends CSI mouse-wheel bytes when the
  pane reports mouse, and scrolls local history otherwise. Clicks follow
  live DECSET. Status **A** / **K** / **M** mark alternate screen,
  keyboard input, and mouse reporting.
- **Tabs.** A broken connection keeps the frozen view, a red tab, and
  **Exit**. Only **Exit** (or closing the chip) removes a tab. Maximize
  hides on a single pane. Rename confirms on Enter. Opening **+**
  re-reads `~/.ssh/config`.
- **Reconnect and resize.** Immediate after laptop suspend. Snapshot
  waits for the SIGWINCH redraw so wrapped lines stay whole.
- **Input.** Ctrl+Backspace is `C-h`. A wrapped `http(s)` URL copies as
  one destination. DECSET 2026 applies atomically; output holds during
  drag-select.
- **SSH.** Shared Sunset client for configuration, trust, and ProxyJump.
  Pinned for agent-held security keys and OpenSSH RSA host signatures.

Prebuilt Linux, macOS, and Windows artifacts are attached below. Linux and
Windows builds are unsigned; macOS builds are ad-hoc codesigned, not notarized.

Known limitations: MFA, host and user certificates, `ProxyCommand`,
`sk-ecdsa` and file-based SK keys, and reusing one SSH connection across
tabs are not supported. Remote hosts are Linux with stock OpenSSH and
tmux. No performance baselines are published yet.

## v0.2.0

Tabs, mouse, paste, session create, SFTP drops, and an About panel after
`v0.1.2`.

- **Quiet tabs.** A healthy connected tab with no view change for `idle`
  seconds (default 30, 0 off) turns blue. Failed tabs keep the yellow
  reconnecting chip and use light-red title text.
- **Copy.** Finishing a drag, double-click, or triple-click copies immediately,
  clears the highlight, and shows **Copied!** on the status bar for a second.
  The Copy button copies the whole pane. Right-click does not copy.
- **Paste.** Clipboard paste is sent as soon as it is requested. Control
  characters other than tabs and line endings are still rejected.
- **Create.** The typed session name appears in the list at once, and Starcom
  attaches when the host confirms it. The last-used session is saved as a form
  target so startup can resume it.
- **Mouse.** Unmodified left clicks are forwarded when the pane asked for mouse
  reports. Drags, modified clicks, and double/triple clicks stay local
  selection. The wheel already went to the application in that case.
- **File drop.** Up to eight files dropped with a connected pane focused upload
  over SFTP into the remote temp directory, then the remote paths are pasted. A
  status-bar progress bar tracks large files, with a Cancel button. Non-regular
  files are rejected. A file larger than 32 MiB asks Yes/No on the status bar
  instead of being discarded. The upload is a separate, cancellable SSH
  connection, so it cannot stall the tmux control channel. Delayed completion
  remains bound to the original pane generation. Wayland binds `wl_data_device`
  itself and reads bounded URI data off the event thread so the compositor stays
  responsive.
- **About.** The tab strip has an About button on the right. It opens a modal
  with the icon, version, GitHub URL, author, total time Starcom has been
  open, and the workspace `fps` / `idle` settings.
- **Composer.** **+** shows the connection form on the plus chip. A tab is
  registered when you press Connect, so an unused New connection chip cannot
  stick in the strip.
- **Startup workspace.** Saved tabs reconnect to their previous hosts and tmux
  sessions automatically by default; **About** has an opt-out for starting
  fresh. The last selected pane and window are restored when they still exist;
  missing or moved panes fall back to a valid visible view. Repeated native
  shutdown callbacks no longer overwrite the saved workspace with the
  already-cleared in-memory tab list.
- **Move pane.** Arrow buttons swap the focused pane with its neighbor.
- **Rename session.** Double-click a connected tab to rename the tmux session.
  The new name is saved immediately so a restart reconnects to it. A rename
  does not rebuild every pane. A rejected name restores the previous one.
- **Zoom scroll.** Maximizing or restoring a pane keeps the local history
  viewport, including when you were following the live tip. A copied offset
  no longer overscrolls or unsticks the tip. Typing and paste jump local
  history back to the live tip. Clicking an OSC 8 hyperlink copies its target
  to the clipboard, since the URL is a cell attribute and not selectable text.
- **OpenTUI frames.** Remote output stays pending until the terminal is
  painted, and a deferred remote wake is forced once the fps interval
  elapses, so an erase-then-redraw (OpenCode and similar) cannot leave a
  blank middle on screen.
- **Terminal polish.** Zoomed panes show a distinct restore icon. Activity uses
  a fixed-size three-dot pulse instead of spinning or shape-shifting glyphs.
  Shift-Enter and Shift-Backspace fall back to Enter and BSpace instead of
  allowing `S-Enter` or `S-BSpace` to appear as literal input. The wheel is
  never a tmux key name (`WheelUp` was typed as text). Shift+wheel always
  scrolls local history. Alternate-screen mouse apps get CSI wheel bytes;
  other application panes get tmux Up/Down; otherwise local history. One
  mouse notch is one remote tick. Dragging pane contents selects; the
  scrollbar still scrolls. The status pulse advances only when the
  selected terminal changes or scrolls; hidden-tab output no longer repaints
  an unchanged selected terminal. Move arrows account for tmux's separator
  cell. Typing `exit` in the last pane closes the tab instead of leaving a
  gray frozen view.
- **History.** The default local history depth is 1000 lines, matching the
  snapshot cap.
- **Sunset.** Pinned to `navigato-rs/sunset` `c245252`, which includes the
  sans-io SFTP client used for drops.

Prebuilt Linux, macOS, and Windows artifacts are attached below. Linux and
Windows builds are unsigned; macOS builds are ad-hoc codesigned, not notarized.

Known limitations: MFA, host and user certificates, `ProxyJump`, `ProxyCommand`,
`sk-ecdsa` and file-based SK keys, and reusing one SSH connection across tabs
are not supported. Remote hosts are Linux with stock OpenSSH and tmux. No
performance baselines are published yet.

## v0.1.2

Terminal viewport and tab chrome after `v0.1.1`.

- **History stays where you left it.** Scrolling up no longer follows new
  output. The live tip is the last screen of the buffer, not past it, so a
  new attach is not a blank pane. A dropped connection keeps that last view
  on screen (dimmed) while it retries.
- **Tabs.** Hover lightens a chip without changing the strip height. Color
  still tracks connected, connecting, and failed.
- **Status.** The focused pane's size sits next to Exit. A copy notice
  replaces the hint for a moment. Idle polls do not wake the GUI unless the
  screen changed.

Prebuilt Linux, macOS, and Windows artifacts are attached below. Linux and
Windows builds are unsigned; macOS builds are ad-hoc codesigned, not notarized.

Known limitations: MFA, host and user certificates, `ProxyJump`, `ProxyCommand`,
`sk-ecdsa` and file-based SK keys, and reusing one SSH connection across tabs
are not supported. Remote hosts are Linux with stock OpenSSH and tmux. No
performance baselines are published yet.

## v0.1.1

Identities, hardware keys, install, and the terminal workspace after `v0.1.0`.

- **Identities, then the agent.** Files from the profile (or OpenSSH's default
  `id_ed25519` / `id_ecdsa` / `id_rsa` when none are set) are offered in order,
  then the local agent unless `IdentitiesOnly` is set. There is no
  agent-versus-key radio. A missing `User` is the local account. `HostKeyAlias`
  is the known-hosts lookup name; TCP still uses `HostName`. Encrypted files and
  unsupported agent algorithms are named in the error (`the SSH agent holds 3
  keys, 1 unsupported (sk-ecdsa-…)`).
- **Agent-held `sk-ssh-ed25519`.** Hardware keys in the local agent are offered;
  signing stays in the authenticator. `sk-ecdsa` and file-based SK keys are not.
- **Saved workspace.** Restored tabs re-read identity policy and blockers from
  `~/.ssh/config`. An unreadable `workspace.conf` is a system dialog before the
  window opens: clear the file, or exit and leave it. Tmux session names are
  not persisted; the host is listed when you pick it.
- **Desktop.** Maximize is tmux zoom and keeps focus. Exit, or typing `exit` in
  the last shell, returns to the form and names the tab **New connection**.
  Tabs are smaller and color-coded: green connected, yellow connecting or
  reconnecting, red failed. Drag a tab to reorder without dropping the pane's
  keyboard focus; switching tabs restores it. Connect shows **Connecting…**
  with a spinner. A reconnecting session paints the last view slightly dimmed.
  Create a session in a field beside the session list, not a dialog. Right-click
  copies the selection or the whole pane. Divider drags always resize tmux.
  After keys or wheel, remote echo runs at 20 fps for a short time. The idle
  paint rate is `fps` in `workspace.conf` (`etc/workspace.conf.example`);
  polls that do not change the screen do not wake the GUI. Hovering a tab
  lightens it without changing the strip height. Scrolling up in history stays
  on those lines while new output arrives; the live tip still follows. The
  status line shows the focused pane's size, and a copy notice replaces the
  hint for a moment. The selected pane is the widget with keyboard focus;
  arrows, Tab, and Escape stay with that field or pane instead of walking to
  other buttons.
- **Install.** `make install` does not assume GNU `install -D`. On macOS it
  writes `~/Applications/Starcom.app`. macOS/Windows icons use a full PNG
  family for `cargo-bundle --bin`.

Prebuilt Linux, macOS, and Windows artifacts are attached below. Linux and
Windows builds are unsigned; macOS builds are ad-hoc codesigned, not notarized.

Known limitations: MFA, host and user certificates, `ProxyJump`, `ProxyCommand`,
`sk-ecdsa` and file-based SK keys, and reusing one SSH connection across tabs
are not supported. Remote hosts are Linux with stock OpenSSH and tmux. No
performance baselines are published yet.

## v0.1.0

First release. Starcom attaches to existing remote tmux sessions over its own
embedded SSH client and draws them in a native window.

- **Attach, don't replace.** Uses tmux control mode (`tmux -N -C`) against a
  stock remote tmux. Starcom never starts a tmux server implicitly, never
  modifies global tmux settings, and leaves the ordinary `tmux` client usable
  as a fallback. Closing a tab or the window detaches; remote jobs keep running.
- **Embedded SSH.** Sunset plus RustCrypto — no OpenSSL, libssh2, ring, AWS-LC,
  or `ssh` subprocess. Strict known-hosts verification with exact,
  port-qualified, and hashed entries; unknown or changed keys always fail.
  Ed25519, RSA/SHA-256, and ECDSA P-256 keys, and Unix and Windows OpenSSH
  agents. Hardware-backed `sk-*` keys are not offered.
- **`~/.ssh/config` reading, without executing anything.** `Host` patterns,
  `HostName`, `User`, `Port`, `IdentityFile`, `IdentitiesOnly`,
  `UserKnownHostsFile`, and bounded `Include`. Directives that cannot be
  honoured exactly are shown as blockers rather than bypassed. Multiple
  `IdentityFile` entries are a blocker in this release.
- **Authentication.** The form chooses an SSH agent or a key file. Encrypted
  files need `ssh-add`.
- **Automatic reconnection, for transport loss only.** Authentication,
  host-key, missing-session, server-exit, and detach failures stop and wait.
  Nothing typed while offline is queued, and no uncertain input is replayed.
  A sleeping laptop or a black-holed control stream is treated as transport
  loss, not as a protocol fault.
- **Session discovery and creation.** Listing uses `tmux -N` and cannot bring a
  server into existence. Creating a session is only reachable from an explicit
  confirmation.
- **Saved tabs.** Destinations and preferences persist; credentials, host-key
  material, and terminal contents never do. Restoring a workspace opens forms,
  not connections.
- **Desktop client.** Blade/egui rendering of Alacritty's terminal model, panes
  and tabs, mouse selection, scrollback, paste confirmation, and draggable pane
  dividers. Remote resizing is opt-in.

Prebuilt Linux, macOS, and Windows artifacts were attached to the GitHub
Release. Linux and Windows builds are unsigned; macOS builds are ad-hoc
codesigned, not notarized.
