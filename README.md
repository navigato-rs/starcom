# Starcom

**Session Terminal And Remote COMmander.**

A small native client for persistent remote tmux sessions. Linux, macOS, and
Windows clients; Linux hosts with stock SSH and tmux. No remote Starcom service.
Built with Rust, Blade, egui, and Alacritty's terminal core.

![Starcom desktop attached to a remote tmux session](etc/screenshot.png)

## Install

Prebuilt binaries are attached to each [GitHub Release](https://github.com/navigato-rs/starcom/releases)
on `v*` tags. Linux and Windows builds are unsigned; macOS builds are ad-hoc
codesigned, not notarized.

| Platform | Artifact |
| --- | --- |
| Linux x86_64 | `.tar.gz`, `.AppImage`, `.deb`, `.rpm` |
| macOS Apple Silicon | `.zip` app bundle, `.dmg` (ad-hoc signed) |
| Windows x86_64 | `.zip` |

From source:

```sh
cargo install --locked --git https://github.com/navigato-rs/starcom
```

That builds the desktop client. Run `starcom` or `starcom --demo`. A recent stable
Rust is required (`rust-version` is 1.96).

The crates.io `starcom` crate is a name reservation and is not a working install.

## Desktop integration

```sh
make install
```

On Linux this puts the binary, `.desktop` entry, and icon under `~/.local`, so
Starcom appears in the application menu. On macOS it also writes
`~/Applications/Starcom.app` (Launchpad and Spotlight); a `.desktop` file is
not a macOS launcher. `$(PREFIX)/bin` is on the PATH in both cases.

To install system-wide instead (`/usr` on Linux, `/Applications` on macOS):

```sh
sudo make install PREFIX=/usr
```

To remove:

```sh
make uninstall
```

## Run

From a checkout:

```sh
cargo run --release --locked
cargo run --release --locked -- --demo
```

Use **+** to open the connection screen. Pick a `Host` from `~/.ssh/config` or type
another destination; Starcom resolves supported user/host/port/key settings and
resolves ProxyJump through Sunset’s shared client and reports unsupported policy
instead of bypassing it.
Choosing a host lists the named windows in Starcom's managed `starcom` tmux
session, then selects the
first available one so **Connect** is the next click. Attached sessions are green
and unavailable, and hosts already connected in this workspace appear first and
in green, including destinations absent from SSH config. Typing is focused
in the new-session field after the host choice. A tab is
registered for that session while it connects and remains available for an
explicit retry after failure. A narrow left sidebar groups session names under
full-width server headers; each header carries that server's latency and pending
input indicator. Each tab is one named window. Tabs on the same resolved server
share one SSH connection, one tmux control client, one reconstructed view, and
one reconnect schedule.

Tabs are saved and resumed automatically by reconnecting once per server and
reopening their named windows; this can be disabled in **Settings**. Startup
uses the same SSH authentication and host-key checks as **Connect**, while the
saved file holds destinations, never credentials.
**Create** is the one action that may start a tmux server. Later creates use
`new-window` on the existing control stream. The same host view lists other tmux
sessions in the same one-row-per-session list. Selecting one replaces
**Connect** with explicit **Migrate** and **Terminate** actions; migration moves
its windows into the managed session and connects to the first one. All
non-managed tmux sessions use this same path, regardless of which tool created
them; they are yellow and show the pane count of each source window. Obsolete
v0.3 saved-tab records no longer create independent runtime
attachments or preselect a host. Switching hosts supersedes an in-flight lookup
immediately, and a late answer cannot replace the new host's result.

The desktop currently supports local scrollback, selection and copying, pane
split/move/zoom/close controls, moving a pane into an automatically named new
logical session/window on the same server, session rename, per-window key/value
notes backed by tmux user options, and opt-in shared tmux pane resizing. Wheel
events go to the application when it asked for mouse reports or
uses the alternate screen; unmodified clicks go only when requested, while
drags stay local selection. Focus a connected pane, then drop up to eight files
onto the window to upload them over SFTP into `/tmp`. Large drops ask before
uploading; an in-flight transfer can be cancelled.
Transport loss reconnects automatically with visible, cancellable backoff;
authentication, trust, and missing-session failures stop and wait for you.
Red tabs distinguish recovery: an unreachable server offers **Reconnect**, while
a reachable server with a missing named window offers **Recreate** for a new
empty window under the saved name.

SSH and cryptography use Rust libraries; OpenSSL is not a build dependency.
Host keys must already be trusted.

[Desktop usage](docs/DESKTOP.md) · [SSH details](docs/SSH.md) ·
[Synchronization limits](docs/SYNCHRONIZATION.md) ·
[session model](docs/SESSION-MODEL.md) · [Roadmap](PLAN.md)

Feedback and private diagnostics: [privacy and reporting](PRIVACY.md).
