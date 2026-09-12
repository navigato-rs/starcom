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
Choosing a known host lists its tmux sessions and selects the first one so
**Connect** is the next click. A tab is registered for that session while it
connects; failed first attachments return to **+** for repair instead of leaving
an empty tab. Each tab shows one window of one session.

Tabs are saved and resumed automatically by reconnecting to their previous host
and tmux session; this can be disabled in **About**. Startup uses the same SSH
authentication and host-key checks as **Connect**, while the saved file holds
destinations, never credentials.
**Create session** is the one action that may start a tmux server, and it asks
first.

The desktop currently supports local scrollback, selection and copying, pane
split/move/zoom/close controls, session rename, and opt-in shared tmux pane
resizing. Wheel events go to the application when it asked for mouse reports or
uses the alternate screen; unmodified clicks go only when requested, while
drags stay local selection. Focus a connected pane, then drop up to eight files
onto the window to upload them over SFTP into `/tmp`. Large drops ask before
uploading; an in-flight transfer can be cancelled.
Transport loss reconnects automatically with visible, cancellable backoff;
authentication, trust, and missing-session failures stop and wait for you.

SSH and cryptography use Rust libraries; OpenSSL is not a build dependency.
Host keys must already be trusted.

[Desktop usage](docs/DESKTOP.md) · [SSH details](docs/SSH.md) ·
[Synchronization limits](docs/SYNCHRONIZATION.md) · [Roadmap](PLAN.md)

Feedback and private diagnostics: [privacy and reporting](PRIVACY.md).
