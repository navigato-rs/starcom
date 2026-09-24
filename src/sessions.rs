//! Explicit session discovery and creation, without changing attach semantics.
//!
//! Listing runs `tmux -N`, so asking what exists can never bring a server into
//! existence. Creating deliberately omits `-N`, because starting a server is the
//! whole point of that action — but it is reachable only from a button the user
//! pressed. Nothing here is ever used as a fallback after a failed attach: an
//! attach that cannot find its session still fails, exactly as before.

use std::{io, time};

use anyhow::Context;

use crate::{command, core, ssh};

const MAX_OUTPUT: usize = 64 * 1024;
const MAX_SESSIONS: usize = 256;

/// What a remote tmux server reports about one session. Names come from the
/// remote host, so they are data: bounded, control-free, and never a command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    pub name: String,
    pub windows: usize,
    pub attached: usize,
}

impl Summary {
    pub fn describe(&self) -> String {
        let windows = if self.windows == 1 {
            "window"
        } else {
            "windows"
        };
        if self.attached == 0 {
            format!("{} {windows}", self.windows)
        } else {
            format!("{} {windows}, attached", self.windows)
        }
    }
}

/// List the sessions on the host. `-N` forbids starting a server, so a host
/// with no tmux running stays that way and lists nothing.
pub fn list(options: &ssh::Options, socket: Option<&str>) -> anyhow::Result<Vec<Summary>> {
    let mut wire = "exec tmux -N".to_owned();
    if let Some(socket) = socket {
        wire.push_str(" -S ");
        wire.push_str(&command::shell_quote(socket)?);
    }
    // Tab-separated: a session name may contain spaces but never a tab, because
    // tmux rejects control characters in names.
    wire.push_str(" list-sessions -F '#{session_name}\t#{session_windows}\t#{session_attached}'");
    let output = run(options, &wire)?;
    // "no sessions" and "no server running" are tmux's way of saying the list
    // is empty. Anything else on stderr is still a failure.
    if output.stdout.is_empty() && host_has_no_sessions(&output.stderr) {
        return Ok(Vec::new());
    }
    parse(&stdout_text(output)?)
}

/// Create a session, then leave. This starts a tmux server when none is running,
/// which is why it exists only behind an explicit action and never runs itself.
/// It does not attach: the caller connects afterwards through the normal path.
pub fn create(
    options: &ssh::Options,
    socket: Option<&str>,
    session: &core::SessionName,
    size: core::Size,
) -> anyhow::Result<()> {
    let mut wire = "exec tmux".to_owned();
    if let Some(socket) = socket {
        wire.push_str(" -S ");
        wire.push_str(&command::shell_quote(socket)?);
    }
    // -d leaves it detached. No command is supplied, so the user's default shell
    // runs, exactly as it would from their own terminal.
    wire.push_str(&format!(
        " new-session -d -s {} -x {} -y {}",
        command::shell_quote(session.as_str())?,
        size.columns(),
        size.rows()
    ));
    stdout_text(run(options, &wire)?).map(|_| ())
}

struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// One bounded, non-PTY command. Separate from the control-mode attachment: it
/// opens its own connection, runs one command, and closes.
fn run(options: &ssh::Options, wire: &str) -> anyhow::Result<CommandOutput> {
    let deadline = time::Instant::now() + options.timeout;
    let mut channel = ssh::Connection::connect(options)?.exec(wire)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut buffer = [0; 8192];
    while !channel.eof() {
        anyhow::ensure!(
            time::Instant::now() < deadline,
            "listing sessions exceeded its deadline"
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
    Ok(CommandOutput { stdout, stderr })
}

/// Stdout from the command. A stderr-only complaint is escaped before it
/// can reach a GUI label.
fn stdout_text(output: CommandOutput) -> anyhow::Result<String> {
    if output.stdout.is_empty() && !output.stderr.is_empty() {
        let detail: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(512)
            .collect::<String>()
            .escape_debug()
            .to_string();
        anyhow::bail!("tmux reported: {detail}");
    }
    String::from_utf8(output.stdout).context("remote output is not UTF-8")
}

/// tmux exits 1 with one of these lines when there is nothing to list.
fn host_has_no_sessions(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(line) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    let line = line.to_ascii_lowercase();
    line == "no sessions"
        || line.starts_with("no server running")
        || error_connecting_to_nothing(&line)
}

fn error_connecting_to_nothing(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("error connecting to ") else {
        return false;
    };
    rest.ends_with("(no such file or directory)") || rest.ends_with("(connection refused)")
}

enum Stream {
    Stdout,
    Stderr,
}

fn parse(output: &str) -> anyhow::Result<Vec<Summary>> {
    let mut sessions = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        anyhow::ensure!(
            sessions.len() < MAX_SESSIONS,
            "host reports more than {MAX_SESSIONS} sessions"
        );
        let mut fields = line.split('\t');
        let (Some(name), Some(windows), Some(attached), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            anyhow::bail!("unexpected session listing from the host");
        };
        // Validate the name the same way an attach target is validated, so a
        // listed session is one that can actually be attached.
        let name = core::SessionName::new(name)
            .context("host listed a session name Starcom cannot target")?;
        sessions.push(Summary {
            name: name.as_str().to_owned(),
            windows: windows.parse().context("invalid window count")?,
            attached: attached.parse().context("invalid attached count")?,
        });
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_parsed_and_bounded() {
        let sessions = parse("work\t3\t1\nbuild\t1\t0\n").unwrap();
        assert_eq!(
            sessions,
            [
                Summary {
                    name: "work".into(),
                    windows: 3,
                    attached: 1
                },
                Summary {
                    name: "build".into(),
                    windows: 1,
                    attached: 0
                }
            ]
        );
        assert_eq!(sessions[0].describe(), "3 windows, attached");
        assert_eq!(sessions[1].describe(), "1 window");
        assert!(parse("").unwrap().is_empty());
        for stderr in [
            "no sessions\n",
            "no server running on /tmp/tmux-1000/default\n",
            "error connecting to /tmp/tmux.sock (No such file or directory)\n",
            "error connecting to /tmp/tmux.sock (Connection refused)\n",
        ] {
            assert!(host_has_no_sessions(stderr.as_bytes()), "{stderr}");
        }
        for stderr in [
            "error connecting to /tmp/tmux.sock (Permission denied)\n",
            "no sessions\ntmux: command not found\n",
            "",
        ] {
            assert!(!host_has_no_sessions(stderr.as_bytes()), "{stderr}");
        }
        let many = "s\t1\t0\n".repeat(MAX_SESSIONS + 1);
        assert!(parse(&many).is_err());
    }

    #[test]
    fn a_hostile_listing_cannot_produce_an_untargetable_session() {
        // Remote text is data. A name Starcom would refuse to target must be
        // refused here too, rather than shown as something the user can pick.
        for line in [
            "work\u{1b}]0;x\u{7}\t1\t0",
            "work\t1",
            "work\t1\t0\textra",
            "work\tnot-a-number\t0",
            "\t1\t0",
        ] {
            assert!(parse(line).is_err(), "accepted {line:?}");
        }
        // A space is fine; tmux allows it and so does SessionName.
        assert_eq!(parse("my work\t1\t0").unwrap()[0].name, "my work");
    }
}
