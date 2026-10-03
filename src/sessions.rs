//! Managed-window discovery, explicit migration, and first-workspace creation.
//!
//! Discovery runs `tmux -N`, so asking what exists can never bring a server
//! into existence. Migration is the only compatibility path: it moves windows
//! from a user-selected non-managed session into Starcom's managed session.

use std::{collections, io, time};

use anyhow::Context;

use crate::{command, core, ssh};

const MAX_OUTPUT: usize = 64 * 1024;
const MAX_WINDOWS: usize = 256;
const MAX_SESSIONS: usize = 256;
pub const MANAGED_SESSION: &str = "starcom";

/// One logical Starcom session: a named window in the managed tmux session.
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

/// One window in a non-managed tmux session. Its index is retained so a
/// partially completed migration derives the same destination name on retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OtherWindow {
    pub id: tmuxctl::WindowId,
    pub index: u32,
    pub name: String,
    pub panes: usize,
}

/// A tmux session Starcom does not own. It is shown as an explicit migration
/// source; discovery alone never changes or attaches to it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OtherSession {
    pub id: tmuxctl::SessionId,
    pub name: String,
    pub windows: Vec<OtherWindow>,
    pub attached: usize,
    pub grouped: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Listing {
    pub managed: Vec<Summary>,
    pub other: Vec<OtherSession>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Migration {
    pub source: String,
    pub windows: Vec<Summary>,
    pub listing: Listing,
}

/// List every session/window on the selected tmux server. `-N` forbids
/// starting a server, so a host with no tmux reports an empty listing.
pub fn discover(options: &ssh::Options, socket: Option<&str>) -> anyhow::Result<Listing> {
    let mut wire = tmux(socket, true)?;
    wire.push_str(
        " list-windows -a -F '#{session_id}\t#{session_name}\t#{session_group}\t\
         #{session_attached}\t#{window_index}\t#{window_id}\t#{window_name}\t#{window_panes}'",
    );
    let output = match run(options, &wire) {
        Ok(output) => output,
        Err(error) => {
            let detail = format!("{error:#}").to_ascii_lowercase();
            if detail.contains("no server running") || detail.contains("error connecting to") {
                return Ok(Listing::default());
            }
            return Err(error);
        }
    };
    parse(&output)
}

/// Create a named window, creating the managed session when it does not exist.
/// This may start a tmux server, which is why it exists only behind an explicit
/// action and never runs itself.
pub fn create(
    options: &ssh::Options,
    socket: Option<&str>,
    window: &core::SessionName,
    size: core::Size,
) -> anyhow::Result<()> {
    let existing = discover(options, socket)?.managed;
    anyhow::ensure!(
        !existing
            .iter()
            .any(|summary| summary.name == window.as_str()),
        "a logical session named '{}' already exists",
        window.as_str()
    );
    let mut wire = tmux(socket, false)?;
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

/// Move every window from a deliberately selected session into the managed
/// session. Linking and verification happen before the source links are
/// removed, so an interrupted first stage leaves the original session usable.
pub fn migrate(
    options: &ssh::Options,
    socket: Option<&str>,
    selected: &OtherSession,
) -> anyhow::Result<Migration> {
    let before = discover(options, socket)?;
    let source = before
        .other
        .iter()
        .find(|session| session.id == selected.id && session.name == selected.name)
        .context("the selected tmux session changed; refresh and try again")?;
    anyhow::ensure!(
        !source.grouped,
        "grouped tmux sessions cannot be migrated safely"
    );
    anyhow::ensure!(
        !source.windows.is_empty(),
        "the selected session has no windows"
    );

    let targets = migration_names(source, &before.managed)?;
    if before.managed.is_empty() {
        // Rename windows first and the session last. If an earlier command
        // fails, the source remains discoverable and the operation is safe to
        // retry; the names are derived from stable source indexes.
        let mut commands = targets
            .iter()
            .map(|(window, name)| rename_window(*window, name))
            .collect::<anyhow::Result<Vec<_>>>()?;
        commands.push(format!(
            "rename-session -t {} {}",
            command::shell_quote(&source.id.to_string())?,
            command::shell_quote(MANAGED_SESSION)?
        ));
        run(options, &command_list(socket, &commands)?)?;
    } else {
        let managed_ids: collections::BTreeSet<_> =
            before.managed.iter().map(|window| window.id).collect();
        // Tmux permits duplicate window names, while the managed session does
        // not. Assign the already validated destination names before linking
        // so the temporary shared-link state also satisfies that invariant.
        // This is retryable because names depend on stable source indexes.
        let renames = targets
            .iter()
            .filter(|(id, name)| {
                source
                    .windows
                    .iter()
                    .find(|window| window.id == *id)
                    .is_some_and(|window| window.name != name.as_str())
            })
            .map(|(window, name)| rename_window(*window, name))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if !renames.is_empty() {
            run(options, &command_list(socket, &renames)?)?;
        }
        let links = source
            .windows
            .iter()
            .filter(|window| !managed_ids.contains(&window.id))
            .map(|window| {
                Ok(format!(
                    "link-window -d -s {} -t {}",
                    command::shell_quote(&window.id.to_string())?,
                    command::shell_quote(&format!("={MANAGED_SESSION}:"))?
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if !links.is_empty() {
            run(options, &command_list(socket, &links)?)?;
        }

        let linked = discover(options, socket)?;
        let linked_ids: collections::BTreeSet<_> =
            linked.managed.iter().map(|window| window.id).collect();
        anyhow::ensure!(
            source
                .windows
                .iter()
                .all(|window| linked_ids.contains(&window.id)),
            "tmux did not link every source window; the original session was left intact"
        );

        let finish = source
            .windows
            .iter()
            .map(|window| {
                let target = format!("{}:{}", source.id, window.id);
                Ok(format!(
                    "unlink-window -t {}",
                    command::shell_quote(&target)?
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        run(options, &command_list(socket, &finish)?)?;
    }

    let listing = discover(options, socket)?;
    let migrated_ids: collections::BTreeSet<_> =
        source.windows.iter().map(|window| window.id).collect();
    anyhow::ensure!(
        !listing.other.iter().any(|session| session.id == source.id),
        "the source session still has windows; refresh and migrate it again"
    );
    let windows: Vec<_> = listing
        .managed
        .iter()
        .filter(|window| migrated_ids.contains(&window.id))
        .cloned()
        .collect();
    anyhow::ensure!(
        windows.len() == migrated_ids.len(),
        "a migrated window is missing from the managed session"
    );
    Ok(Migration {
        source: source.name.clone(),
        windows,
        listing,
    })
}

/// Terminate one deliberately selected non-managed session. Re-discover the
/// source immediately before killing it so a recycled tmux ID or renamed
/// session cannot turn a stale UI click into a different destructive action.
pub fn terminate(
    options: &ssh::Options,
    socket: Option<&str>,
    selected: &OtherSession,
) -> anyhow::Result<Listing> {
    let before = discover(options, socket)?;
    let source = before
        .other
        .iter()
        .find(|session| session.id == selected.id && session.name == selected.name)
        .context("the selected tmux session changed; refresh and try again")?;
    let command = format!(
        "kill-session -t {}",
        command::shell_quote(&source.id.to_string())?
    );
    run(options, &command_list(socket, &[command])?)?;
    let after = discover(options, socket)?;
    anyhow::ensure!(
        !after.other.iter().any(|session| session.id == source.id),
        "the selected tmux session still exists"
    );
    Ok(after)
}

fn migration_names(
    source: &OtherSession,
    managed: &[Summary],
) -> anyhow::Result<Vec<(tmuxctl::WindowId, core::SessionName)>> {
    let managed_by_id: collections::BTreeMap<_, _> = managed
        .iter()
        .map(|window| (window.id, window.name.as_str()))
        .collect();
    let mut used: collections::BTreeSet<String> =
        managed.iter().map(|window| window.name.clone()).collect();
    let many = source.windows.len() > 1;
    let mut targets = Vec::with_capacity(source.windows.len());
    for window in &source.windows {
        if let Some(name) = managed_by_id.get(&window.id) {
            targets.push((window.id, core::SessionName::new((*name).to_owned())?));
            continue;
        }
        let base = if many {
            format!("{}/{}", source.name, window.index)
        } else {
            source.name.clone()
        };
        let mut name = base.clone();
        let mut suffix = 2_u32;
        while used.contains(&name) {
            name = format!("{base} ({suffix})");
            suffix = suffix
                .checked_add(1)
                .context("session-name suffix exhausted")?;
        }
        let name = core::SessionName::new(name)?;
        used.insert(name.as_str().to_owned());
        targets.push((window.id, name));
    }
    Ok(targets)
}

fn rename_window(window: tmuxctl::WindowId, name: &core::SessionName) -> anyhow::Result<String> {
    Ok(format!(
        "rename-window -t {} {}",
        command::shell_quote(&window.to_string())?,
        command::shell_quote(name.as_str())?
    ))
}

fn tmux(socket: Option<&str>, no_start: bool) -> anyhow::Result<String> {
    let mut wire = if no_start {
        "exec tmux -N".to_owned()
    } else {
        "exec tmux".to_owned()
    };
    if let Some(socket) = socket {
        wire.push_str(" -S ");
        wire.push_str(&command::shell_quote(socket)?);
    }
    Ok(wire)
}

fn command_list(socket: Option<&str>, commands: &[String]) -> anyhow::Result<String> {
    anyhow::ensure!(!commands.is_empty(), "empty tmux command list");
    let mut wire = tmux(socket, true)?;
    wire.push(' ');
    wire.push_str(&commands.join(" \\; "));
    Ok(wire)
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

fn parse(output: &str) -> anyhow::Result<Listing> {
    let mut managed = Vec::new();
    let mut other = collections::BTreeMap::<u32, OtherSession>::new();
    let mut managed_ids = collections::BTreeSet::new();
    let mut managed_names = collections::BTreeSet::new();
    let mut rows = 0_usize;
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        rows += 1;
        anyhow::ensure!(
            rows <= MAX_WINDOWS,
            "tmux reports more than {MAX_WINDOWS} windows"
        );
        let mut fields = line.split('\t');
        let (
            Some(session_id),
            Some(session_name),
            Some(session_group),
            Some(attached),
            Some(index),
            Some(window_id),
            Some(window_name),
            Some(panes),
            None,
        ) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        )
        else {
            anyhow::bail!("unexpected tmux session listing");
        };
        let session_name = core::SessionName::new(session_name)
            .context("host listed a session name Starcom cannot target")?;
        let window_name = core::SessionName::new(window_name)
            .context("host listed a window name Starcom cannot target")?;
        let session = session_id
            .strip_prefix('$')
            .context("invalid tmux session id")?
            .parse::<u32>()
            .context("invalid tmux session id")?;
        let window = tmuxctl::WindowId(
            window_id
                .strip_prefix('@')
                .context("invalid tmux window id")?
                .parse()?,
        );
        let index = index.parse().context("invalid tmux window index")?;
        let panes = panes.parse().context("invalid pane count")?;
        let attached = attached.parse().context("invalid attached-client count")?;
        if session_name.as_str() == MANAGED_SESSION {
            anyhow::ensure!(managed_ids.insert(window), "duplicate managed window id");
            anyhow::ensure!(
                managed_names.insert(window_name.as_str().to_owned()),
                "duplicate managed window name"
            );
            managed.push(Summary {
                id: window,
                name: window_name.as_str().to_owned(),
                panes,
            });
        } else {
            let entry = other.entry(session).or_insert_with(|| OtherSession {
                id: tmuxctl::SessionId(session),
                name: session_name.as_str().to_owned(),
                windows: Vec::new(),
                attached,
                grouped: !session_group.is_empty(),
            });
            anyhow::ensure!(
                entry.name == session_name.as_str()
                    && entry.attached == attached
                    && entry.grouped != session_group.is_empty(),
                "inconsistent tmux session listing"
            );
            anyhow::ensure!(
                !entry.windows.iter().any(|existing| existing.id == window),
                "duplicate window in tmux session"
            );
            entry.windows.push(OtherWindow {
                id: window,
                index,
                name: window_name.as_str().to_owned(),
                panes,
            });
        }
    }
    anyhow::ensure!(
        other.len() <= MAX_SESSIONS,
        "tmux reports more than {MAX_SESSIONS} sessions"
    );
    Ok(Listing {
        managed,
        other: other.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "$0\tstarcom\t\t1\t0\t@3\twork\t2\n\
                           $1\tzork/0\t\t0\t0\t@4\tbash\t1\n\
                           $1\tzork/0\t\t0\t2\t@5\tlogs\t1\n";

    #[test]
    fn all_sessions_are_parsed_and_partitioned() {
        let listing = parse(LISTING).unwrap();
        assert_eq!(
            listing.managed,
            [Summary {
                id: tmuxctl::WindowId(3),
                name: "work".into(),
                panes: 2,
            }]
        );
        assert_eq!(listing.managed[0].describe(), "2 panes");
        assert_eq!(listing.other.len(), 1);
        assert_eq!(listing.other[0].name, "zork/0");
        assert_eq!(listing.other[0].windows.len(), 2);
        assert_eq!(listing.other[0].windows[1].index, 2);
        assert_eq!(parse("").unwrap(), Listing::default());
    }

    #[test]
    fn migration_names_are_stable_and_non_conflicting() {
        let listing = parse(LISTING).unwrap();
        let mut single = listing.other[0].clone();
        single.windows.truncate(1);
        assert_eq!(
            migration_names(&single, &[]).unwrap()[0].1.as_str(),
            "zork/0"
        );
        let targets = migration_names(
            &listing.other[0],
            &[
                Summary {
                    id: tmuxctl::WindowId(9),
                    name: "zork/0/0".into(),
                    panes: 1,
                },
                Summary {
                    id: tmuxctl::WindowId(5),
                    name: "already linked".into(),
                    panes: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(targets[0].1.as_str(), "zork/0/0 (2)");
        assert_eq!(targets[1].1.as_str(), "already linked");
    }

    #[test]
    fn hostile_or_ambiguous_listings_are_rejected() {
        for line in [
            "$0\tstarcom\t\t0\t0\t@1\twork\u{1b}]0;x\u{7}\t1",
            "$0\tstarcom\t\t0\t0\t@1\twork",
            "$0\tstarcom\t\t0\t0\t@1\twork\t1\textra",
            "$0\tstarcom\t\t0\t0\t@1\twork\tnot-a-number",
            "$0\tstarcom\t\t0\t0\t@1\t\t1",
            "$0\tstarcom\t\t0\t0\t@1\twork\t1\n$0\tstarcom\t\t0\t1\t@2\twork\t1",
            "$0\tstarcom\t\t0\t0\t@1\twork\t1\n$0\tstarcom\t\t0\t1\t@1\tbuild\t1",
        ] {
            assert!(parse(line).is_err(), "accepted {line:?}");
        }
    }
}
