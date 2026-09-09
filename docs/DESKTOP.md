# Desktop client

Starcom has an experimental **interactive** desktop client. It attaches to an
existing tmux session, reconstructs each pane into an Alacritty terminal model,
and presents tmux windows and panes through Blade/egui.

The implementation is usable enough for focused testing, but it is not yet a
finished terminal: application mouse drags, broad TUI compatibility, and native
macOS/Windows interaction acceptance remain open.

## Start

```sh
cargo run --release --locked
cargo run --release --locked -- --demo
```

The demo is local synthetic data. It does not open SSH, read credentials, or
attach to a tmux server.

## Connection tabs

A Starcom tab owns one connection form, one SSH/tmux client, one terminal view,
and its pending input tokens. Use **+** or Ctrl-Shift-T (Cmd-T on macOS) to open
the connection form on the plus chip itself. A registered tab is added only when
you press **Connect**. Drag a tab to reorder the strip. **About** on the right of
the strip opens a modal with the icon, version, GitHub URL, total time Starcom
has been open, and the workspace `fps` / `idle` settings. The form is never a
sidebar beside another session's panes.

After a successful connection, the terminal workspace replaces the form in that
tab. A failed first attach stays on the form and shows why. **Exit**, and typing
`exit` in the last shell, return to the form in the same tab. An empty form is
dropped instead of leaving a stuck New connection chip; a named destination
keeps its label so you can reconnect. Restored tabs that have not been attached
this session keep their destination labels. Tabs are green while connected,
including while a pane layout is rebuilt. They turn yellow while connecting or
reconnecting, and use a yellow background with a light-red title after a failure.
Ctrl-Shift-W/Cmd-W closes the tab and detaches that Starcom client; the tmux
server and remote jobs continue running.

A Starcom tab is one tmux session and shows one window of that session. A
window picker is not in this increment. Double-click a connected tab to rename
that tmux session. Enter confirms, Escape or clicking away cancels. A name
already in use is reported without dropping the attachment.

Each tab currently opens its own SSH connection. Reusing a host connection across
multiple session tabs is deferred until it can be done without coupling failures
or authentication state between tabs.

## SSH configuration

The form's main choice is the host. Literal `Host` aliases from `~/.ssh/config`
are listed for one-click selection; the field after those buttons accepts a
hostname, address, or alias that is not in that list. Selecting a known host
resolves the supported
profile and lists that host's tmux sessions, selecting the first so **Connect**
is available immediately. Startup resume attaches directly to each saved
session without listing first, using the current SSH configuration.

Currently supported:

- `Host` patterns and negation, with OpenSSH-style first-value-wins ordering;
- `HostName`, `User` (or the local account if omitted), `Port`, and
  `HostKeyAlias` (known-hosts lookup name; the TCP target is still `HostName`);
- every `IdentityFile` in order, plus `IdentitiesOnly`; if none are set, every
  existing `~/.ssh/id_ed25519` / `id_ecdsa` / `id_rsa`;
- one `UserKnownHostsFile`;
- bounded `Include` files and `*`/`?` include globs.

Wildcard `Host` entries apply defaults but are not shown as literal suggestions.
The parser never executes configuration commands. `Match`, `ProxyJump`,
`ProxyCommand`, certificates, custom agents, algorithm overrides, and other
routing/security policy are displayed as blockers. Starcom does not silently
bypass them by connecting directly or choosing another key. Hardware-backed
`sk-ecdsa-*` keys are skipped at authentication with a named algorithm, not
treated as a config blocker. Agent-held `sk-ssh-ed25519` is offered.

Use **Reload config** after editing the file. Unknown or changed host keys fail
instead of being accepted automatically. See [SSH.md](SSH.md) for the exact trust
and authentication policy.

## Saved tabs

Open tabs are saved to `~/.config/starcom/workspace.conf` (`%APPDATA%\starcom\`
on Windows, or `$XDG_CONFIG_HOME` where it is set) and reopened next time.

What is saved is where a tab points and how it should connect: destination alias,
host, user, port, tmux socket, last-used session name, selected window and pane,
history depth, whether it is interactive, whether it reconnects, an extra
identity path if you typed one, the global redraw cap (`fps`, default 5), and
how long a quiet connected tab waits before its chip turns blue (`idle`, default
30 seconds, 0 off; see `etc/workspace.conf.example`). Both `fps` and `idle` are
also on the **About** panel. The last-used session is the target used to resume
that tab on startup.
Nothing that would let a reader of that file connect is written: no keys, no
passphrases, no host-key material, and no terminal contents. An identity entry
is the path you already chose, never the key behind it.

Saved tabs resume automatically by default: each reconnects to its previous host
and existing tmux session, and the active tab shows connection progress until
its terminal view is rebuilt. Disable **Resume open tabs on startup** in
**About** to begin future launches with an empty workspace instead. Identity
files, `IdentitiesOnly`, and unsupported-policy blockers are re-read from
`~/.ssh/config`, and normal host-key and authentication checks still apply. A
tab whose saved settings are incomplete or no longer allowed stays on its form
and shows the error; Starcom never silently creates a replacement session. A
saved pane is used only if it still exists in the fresh tmux snapshot. If it
was closed, Starcom selects a visible pane in the saved window, or the first
available window if that window also disappeared. A pane moved to another
window is followed there. A saved file that cannot be read is a system dialog
before the window opens: it names the file and the parse error, and asks whether
to clear the file or exit.
Exit is the default and leaves the file unchanged. Clearing deletes it and
starts fresh.

The demo neither reads nor writes this file.

## Finding and creating sessions

Selecting a known host lists its sessions automatically. **Refresh** asks again.
The query runs `tmux -N`, so it can never bring a tmux server into existence: a
host with no tmux running says so. The last session this tab attached to is selected when it is still on the host;
otherwise the first name in the list. Choosing another only fills the field, and
double-clicking attaches.

Starcom also asks on its own when a connection fails because that session does
not exist. You have already asked to connect and already authenticated, and the
list is exactly the missing information. No other failure triggers it —
authentication and host-key failures could not list anyway, and the others
already say what happened.

The **new session** field beside the list, then **Create**, makes that session on
the host and attaches as soon as tmux confirms it. The typed name appears in the
list immediately. This starts a tmux server if none is running. It is the only
path in Starcom that may start one. No failure anywhere else falls back to it:
an attach that cannot find its session still fails, exactly as before.

Both run on the tab's worker over their own short-lived connection, so neither
blocks the window or disturbs a live attachment.

## Terminal input

Enable **Allow terminal input** before connecting for an interactive attachment.
A read-only attachment remains available for inspection.

Selecting a Host alias offers its `IdentityFile` entries in order, then the
local SSH agent, unless `IdentitiesOnly` is set. If the profile names no files,
Starcom offers every existing default it can sign (`~/.ssh/id_ed25519`,
`id_ecdsa`, `id_rsa`) before the agent. There is no agent-versus-key radio; an
extra identity path lives under Advanced. If no agent is reachable and no
identity files exist, the form says so before you connect. A desktop session
often does not inherit `SSH_AUTH_SOCK` from a shell, so an agent that works in
your terminal may be invisible here; start an agent in the session that launches
Starcom, or add `IdentityFile` to `~/.ssh/config`. The warning clears on its own
once an agent appears; there is no need to restart Starcom.

Click a pane to focus it. Text and committed IME input are encoded as UTF-8.
Enter, arrows, Home/End, Page Up/Down, Insert/Delete, Tab, Escape, Backspace, and
F1-F20 are sent through tmux's key handling. Plain terminal control combinations
such as Ctrl-C, Ctrl-X, Ctrl-V, and Ctrl-Z remain application input.
Traditional terminal input has no portable distinct Shift-Enter encoding, so
Starcom sends it as Enter instead of letting an extended `S-Enter` key name
surface as literal text.

Local clipboard shortcuts are:

- Ctrl-Shift-C / Ctrl-Shift-V on Linux and Windows;
- Cmd-C / Cmd-V on macOS;
- the **Copy** button on the status bar, which copies the whole pane;
- finishing a drag, double-click, or triple-click selection, which copies
  immediately, clears the highlight, and shows **Copied!** on the status bar
  for a second.

Paste is sent as soon as it is requested. It still rejects escape/C1 and other
control characters except tabs and line endings; deliberate control input must
come from key handling, not clipboard text.

Input is bound to the exact connection epoch, reconstructed-view generation, and
pane identity in which it originated. Layout changes, reconnects, tab changes, or
cancellation invalidate stale actions. Starcom does not queue input while offline
and never automatically retries a request whose delivery is uncertain.

`send-keys` can broadcast when a tmux window has `synchronize-panes` enabled.
Starcom detects that state in the server-side action guard and blocks the action
rather than changing the user's option or risking broadcast.

## Scrolling, selection, and copying

Wheel handling follows the pane, not a global shortcut. If the application
enabled mouse reporting (DEC 1000/1002/1003), the wheel is sent as a mouse
report at the hovered cell. If it is on the alternate screen without mouse
reporting, the wheel becomes Up/Down, matching xterm alternate-scroll. Otherwise
the wheel examines local terminal history. One tick is 40 points, one egui
line; leftover smoothing after a notch is accumulated so it cannot add extra
ticks.

Unmodified left clicks are forwarded the same way when the pane asked for mouse
reports: a press and a release at the cell. Shift/Ctrl/Alt/Cmd clicks, drags,
double-clicks, and triple-clicks stay local. Drag to select, double-click for a
word, and triple-click for a line. Selection anchors live in the terminal
model, so they follow incoming scrolls. Copying handles wide cells, combining
characters, and soft wraps, with a 1 MiB output limit.

Focus a connected pane, then drop up to eight files onto the window to upload
them over SFTP into the remote temp directory (`/tmp`), under unique
`starcom-…` names. The remote paths are then pasted into the focused pane. A
progress bar sits in the status bar while a large file is in flight, with a
**Cancel** button. Non-regular files are rejected. A file larger than 32 MiB
asks **Yes** / **No** on the status bar instead of being discarded. Switching
tabs or closing the form cancels the upload; delayed completion never pastes
into a replacement pane after a reconnect or layout change. The upload uses
its own SSH connection, so it cannot stall the tmux control channel. On Wayland,
Starcom binds
`wl_data_device` itself because winit 0.30 does not; the bounded URI-list read
runs off the event thread.

The renderer paints only visible history rows. Font-size controls change the
cell metrics used both to paint and to tell tmux this client's size, so the
remote grid matches the glyphs on screen.

The status bar's bottom-left three-dot pulse advances when the selected terminal
contents refresh or the user scrolls it; it stays still when that panel is idle.
The same fixed-size pulse appears in a yellow connecting tab without changing
the chip's dimensions. Next to the status mark, when a recent small tmux command has
finished, its round-trip time is shown. That is not a probe: nothing extra is
sent to measure it.

Remote pane output is redrawn at most 5 times per second by default (`fps` in
`workspace.conf`; `etc/workspace.conf.example` is the documented file). Buttons,
hover, typing, and other local UI stay immediate. After keys or wheel are sent,
remote frames run at up to 20 fps for a short time so the echo does not wait on
the idle cap. Output in a hidden tab keeps that tab's activity/quiet state
accurate but does not repaint an unchanged selected terminal. A remote wake
stays pending until the selected terminal is actually painted, and a deferred
remote redraw is forced once the fps interval elapses, so an application that
erases then redraws (OpenCode and other OpenTUI apps) cannot leave the empty
erase on screen.

## Pane controls

Each interactive pane has window-style buttons in its top-right corner:

- split right (`split-window -h`)
- split below (`split-window -v`)
- move left / right / up / down (`swap-pane` with the neighbor that shares
  that edge). Hidden when there is no neighbor on that side.
- maximize / restore (`resize-pane -Z`); the icon changes to overlapping
  rectangles while the pane is maximized
- close the pane (`kill-pane`), hidden when it is the last pane in the
  window

Maximize is tmux zoom. Zoomed tmux still lists every pane, overlapping; Starcom
shows the filling pane and keeps focus until you restore. The local history
viewport is kept across maximize and restore, so scrolling up to read and then
zooming does not jump back to the live tip.

Click a pane to focus it (`select-pane`). These are the same tmux operations an
ordinary client would use, so other attached clients see them.

## Pane dividers

Dragging a divider previews the split locally. On release, an interactive
connection sends `resize-pane` to tmux — the same shared layout change a normal
tmux client would make — then reconstructs from the server's geometry. Cells
stay at the real font size; they are not scaled.

Nested or unusual layouts that cannot be mapped to a safe tmux boundary stay
local-only.

Starcom never changes tmux's global sizing options. Zoomed windows, stale geometry,
panes in modes, dead/input-disabled panes, and changed session/window identities
block the resize transaction.

## Disconnect and exit behavior

**Exit** returns to the connection form. The form fields stay so you can
reconnect. An empty form is closed instead of remaining as a New connection
chip. Typing `exit` in the last pane of the session does the same. It is
available in every connection phase, including **Connection failed**.

**Reconnect automatically after connection loss** is on by default in the
connection form. Only transport loss is retried. That includes a TCP drop, a
tmux write/reply deadline (a black-holed stream), and this machine sleeping
(wall time running ahead of the monotonic clock). Authentication failures, host-key
failures, a missing or destroyed session, a tmux server that exited, and an
explicit detach all stop and wait for you, because retrying any of them would
either loop on a security decision or quietly attach somewhere else. While a
retry is waiting, the last view stays on screen in gray so it reads as frozen,
not live, and the tab is yellow with an animated three-dot pulse.

Each retry waits a little longer, up to 30 seconds, with jitter so several tabs
that drop together do not reconnect in lockstep. The status bar shows the attempt
number and the remaining wait, and **Stop reconnecting** ends the schedule at any
time. A successful attachment resets the schedule.

While disconnected, the last view stays readable and copyable, but no input token
is issued: nothing typed during an outage is queued for later delivery, and a
write whose delivery was uncertain is never repeated. Each attempt reattaches
under a new connection epoch and rebuilds every pane model from a fresh snapshot.

Because attaching resolves the session by name, a restarted tmux server can hand
back a different session that merely shares that name. Starcom compares the
session identity across the reconnect and says so when it changed. A pane on
its alternate screen, or tmux retaining more history than the History setting,
is not a warning: those are how full-screen programs and the history cap work.

Window closure disables repaint callbacks, invalidates and wakes all workers, and
releases Blade surface/painter/encoder resources while the winit event loop and
native display handles are still alive. The Linux/X11 smoke test closes through
`WM_DELETE_WINDOW` and requires a zero exit status. This verifies the tested X11
path; native Wayland, macOS, and Windows close-path acceptance is still needed.

## Rendering and terminal limits

The GUI uses Blade, blade-egui, egui 0.34, egui-winit 0.34, and winit 0.30. It does
not use eframe, GPUI, a web view, Alacritty's renderer, or a local PTY.

Initial rendering uses egui's bundled fonts. Platform font discovery, complex
shaping, bidi text, accessibility, and broad IME testing remain future work.
Starcom's snapshot is also not a complete tmux terminal checkpoint; see
[SYNCHRONIZATION.md](SYNCHRONIZATION.md) for unexported parser/pen state and
history limitations.

A full-screen application's conversation history is not necessarily terminal
scrollback. Both tmux and Starcom retain bounded history and cannot recover data
that the server discarded.

## Desktop integration

`make install` builds a release binary and copies it to `$(PREFIX)/bin`
(`~/.local` by default, or `/usr` for a system package). Parent directories are
created with `mkdir -p` so BSD `install` on macOS works; GNU `install -D` is not
assumed.

On Linux it also installs `etc/starcom.desktop` and `etc/starcom.svg`.
`StartupWMClass=starcom` matches the X11/Wayland class the window sets. The same
files are packed into the `.deb` and `.rpm` release artifacts.

On macOS Launchpad does not read `.desktop` files. `scripts/macos-app.sh` builds
`Starcom.app` from the release binary, `etc/macos/Info.plist`, and the PNG icon
family (`iconutil` + ad-hoc `codesign`, no cargo-bundle). The bundle goes to
`~/Applications` when `PREFIX` is under `$HOME`, and `/Applications` otherwise.

Window chrome uses `etc/macos/icon.png`; the Windows PE resource uses
`etc/windows/icon.ico`. Regenerate both from `scripts/generate-icons.py` if the
SVG geometry changes. macOS `.app` / `.dmg` and the Linux AppImage come from
`cargo-bundle` metadata in `Cargo.toml`. GitHub Release jobs run on `v*` tags;
those builds are not notarized or Authenticode-signed.

## Build and validation

The default features include desktop and embedded SSH:

```sh
cargo fmt --check
python scripts/check-dependencies.py
cargo build --locked --all-features --all-targets
cargo test --locked --all-features --all-targets
cargo clippy --locked --all-features --all-targets -- -D warnings
```

The headless paths remain available:

```sh
cargo test --locked --no-default-features --all-targets
cargo run --locked --no-default-features -- --replay tests/data/two-panes.tmux
cargo run --locked --no-default-features --features ssh --bin starcom-inspect -- --help
```

Linux CI runs a disposable loopback sshd and isolated tmux server. It exercises
real interactive input, paste-once behavior, pane resizing/resynchronization,
`synchronize-panes` blocking, process preservation, snapshot continuity,
automatic reconnection after an abrupt transport loss, refusal to reattach to a
destroyed session, and timeouts. The fixture asserts how many tests actually ran,
so a renamed or cfg-excluded test fails instead of passing silently. A native X11/Xvfb test covers rendering, selection, the system clipboard,
window resize, and normal-window clean close. Windows CI tests its OpenSSH-agent
named pipe. macOS and Windows compile/test the full desktop but do not yet have
native GUI automation.

Generate the real application-rendered demo image without SSH or a display server:

```sh
cargo run --locked -- --demo --snapshot target/test-artifacts/desktop.png
```
