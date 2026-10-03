//! Explicit managed-window discovery and first-workspace creation.
//!
//! Listing runs `tmux -N`, so asking what exists can never bring a server into
//! existence. Creating deliberately omits `-N`, because starting a server is the
//! whole point of that action — but it is reachable only from a button the user
//! pressed. Nothing here is ever used as a fallback after a failed attach: an
//! attach that cannot find its session still fails, exactly as before.

use std::{collections, io, time};

use anyhow::Context;

use crate::{command, core, ssh};

const MAX_OUTPUT: usize = 64 * 1024;
const MAX_WINDOWS: usize = 256;
pub const MANAGED_SESSION: &str = "starcom";

/// What the managed tmux session reports about one logical session/window. Names come from the
/// remote host, so they are data: bounded, control-free, and never a command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    pub id: tmuxctl::WindowId,
    pub name: String,
    pub panes: usize,
}

impl Summary {
    pub fn describe(&self) -> String {
        let panes = if self.panes == 1 { "pane" } else { "panes" };
        format!("{} {panes}", self.panes)
    }
}

/// List windows in Starcom's managed session. `-N` forbids starting a server, so a host
/// with no tmux running reports that instead of gaining one.
pub fn list(options: &ssh::Options, socket: Option<&str>) -> anyhow::Result<Vec<Summary>> {
    let mut wire = "exec tmux -N".to_owned();
    if let Some(socket) = socket {
        wire.push_str(" -S ");
        wire.push_str(&command::shell_quote(socket)?);
    }
    // Tab-separated: a managed window name may contain spaces but never a tab, because
    // tmux rejects control characters in names.
    wire.push_str(&format!(
        " list-windows -t {} -F '#{{window_id}}\t#{{window_name}}\t#{{window_panes}}'",
        command::shell_quote(&format!("={MANAGED_SESSION}"))?
    ));
    let output = match run(options, &wire) {
        Ok(output) => output,
        Err(error) => {
            let detail = format!("{error:#}").to_ascii_lowercase();
            if detail.contains("can't find session")
                || detail.contains("no server running")
                || detail.contains("error connecting to")
            {
                return Ok(Vec::new());
            }
            return Err(error);
        }
    };
    parse(&output)
}

/// Create a named window, creating the managed session when it does not exist.
/// This may start a tmux server, which is why it exists only behind an explicit
/// action and never runs itself. It does not attach: the caller connects
/// afterwards through the normal path.
pub fn create(
    options: &ssh::Options,
    socket: Option<&str>,
    window: &core::SessionName,
    size: core::Size,
) -> anyhow::Result<()> {
    let existing = list(options, socket)?;
    anyhow::ensure!(
        !existing
            .iter()
            .any(|summary| summary.name == window.as_str()),
        "a logical session named '{}' already exists",
        window.as_str()
    );
    let mut wire = "exec tmux".to_owned();
    if let Some(socket) = socket {
        wire.push_str(" -S ");
        wire.push_str(&command::shell_quote(socket)?);
    }
    // -d leaves it detached. No command is supplied, so the user's default shell
    // runs, exactly as it would from their own terminal.
    if existing.is_empty() {
        wire.push_str(&format!(
            " new-session -d -s {} -n {} -x {} -y {}",
            command::shell_quote(MANAGED_SESSION)?,
            command::shell_quote(window.as_str())?,
            size.columns(),
            size.rows()
        ));
    } else {
        wire.push_str(&format!(
            " new-window -d -t {} -n {}",
            command::shell_quote(&format!("={MANAGED_SESSION}"))?,
            command::shell_quote(window.as_str())?,
        ));
    }
    run(options, &wire).map(|_| ())
}

/// One bounded, non-PTY command. Separate from the control-mode attachment: it
/// opens its own connection, runs one command, and closes.
fn run(options: &ssh::Options, wire: &str) -> anyhow::Result<String> {
    let deadline = time::Instant::now() + options.timeout;
    let mut channel = ssh::Connection::connect(options)?.exec(wire)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut buffer = [0; 8192];
    while !channel.eof() {
        anyhow::ensure!(
            time::Instant::now() < deadline,
            "remote tmux command exceeded its deadline"
        );
        let mut progressed = false;
        for target in [Stream::Stdout, Stream::Stderr] {
            let read = match target {
                Stream::Stdout => io::Read::read(&mut channel, &mut buffer),
                Stream::Stderr => channel.read_stderr(&mut buffer),
            };
            match read {
                Ok(0) => {}
                Ok(count) => {
                    progressed = true;
                    let sink = match target {
                        Stream::Stdout => &mut stdout,
                        Stream::Stderr => &mut stderr,
                    };
                    anyhow::ensure!(
                        sink.len() + count <= MAX_OUTPUT,
                        "remote output exceeds {MAX_OUTPUT} bytes"
                    );
                    sink.extend_from_slice(&buffer[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("read remote output"),
            }
        }
        if !progressed {
            channel.wait(deadline)?;
        }
    }
    if stdout.is_empty() && !stderr.is_empty() {
        // tmux says "no server running on ..." here. Escape it: this is remote
        // text on its way to a GUI label, not to a terminal.
        let detail: String = String::from_utf8_lossy(&stderr)
            .chars()
            .take(512)
            .collect::<String>()
            .escape_debug()
            .to_string();
        anyhow::bail!("tmux reported: {detail}");
    }
    String::from_utf8(stdout).context("remote output is not UTF-8")
}

enum Stream {
    Stdout,
    Stderr,
}

fn parse(output: &str) -> anyhow::Result<Vec<Summary>> {
    let mut windows = Vec::new();
    let mut ids = collections::BTreeSet::new();
    let mut names = collections::BTreeSet::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        anyhow::ensure!(
            windows.len() < MAX_WINDOWS,
            "managed session reports more than {MAX_WINDOWS} windows"
        );
        let mut fields = line.split('\t');
        let (Some(id), Some(name), Some(panes), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            anyhow::bail!("unexpected window listing from the host");
        };
        // Validate the name the same way an attach target is validated, so a
        // listed session is one that can actually be attached.
        let name = core::SessionName::new(name)
            .context("host listed a session name Starcom cannot target")?;
        let id = tmuxctl::WindowId(
            id.strip_prefix('@')
                .context("invalid managed window id")?
                .parse()?,
        );
        anyhow::ensure!(ids.insert(id), "duplicate managed window id");
        anyhow::ensure!(
            names.insert(name.as_str().to_owned()),
            "duplicate managed window name"
        );
        windows.push(Summary {
            id,
            name: name.as_str().to_owned(),
            panes: panes.parse().context("invalid pane count")?,
        });
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_parsed_and_bounded() {
        let windows = parse("@3\twork\t2\n@4\tbuild\t1\n").unwrap();
        assert_eq!(
            windows,
            [
                Summary {
                    id: tmuxctl::WindowId(3),
                    name: "work".into(),
                    panes: 2,
                },
                Summary {
                    id: tmuxctl::WindowId(4),
                    name: "build".into(),
                    panes: 1,
                }
            ]
        );
        assert_eq!(windows[0].describe(), "2 panes");
        assert_eq!(windows[1].describe(), "1 pane");
        assert!(parse("").unwrap().is_empty());
        let many = "@1\ts\t1\n".repeat(MAX_WINDOWS + 1);
        assert!(parse(&many).is_err());
    }

    #[test]
    fn a_hostile_listing_cannot_produce_an_untargetable_session() {
        // Remote text is data. A name Starcom would refuse to target must be
        // refused here too, rather than shown as something the user can pick.
        for line in [
            "@1\twork\u{1b}]0;x\u{7}\t1",
            "@1\twork",
            "@1\twork\t1\textra",
            "@1\twork\tnot-a-number",
            "@1\t\t1",
            "@1\twork\t1\n@2\twork\t1",
            "@1\twork\t1\n@1\tbuild\t1",
        ] {
            assert!(parse(line).is_err(), "accepted {line:?}");
        }
        // A space is fine; tmux allows it and so does SessionName.
        assert_eq!(parse("@1\tmy work\t1").unwrap()[0].name, "my work");
    }
}
