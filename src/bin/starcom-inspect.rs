//! Small live-transport diagnostic, not the planned desktop interface.
use std::{collections, env, ffi, path, process, time};

use anyhow::Context;
use starcom::{core, inspect, session, snapshot, ssh};

const HELP: &str = "Starcom SSH/tmux inspector (read-only; no GUI)\n\
\n\
Usage: starcom-inspect --host HOST --user USER --session NAME --known-hosts FILE\n\
                      [--identity FILE]... [--agent] [--port PORT] [--socket PATH]\n\
                      [--history LINES] [--timeout SECONDS] [--watch SECONDS]\n\
\n\
HOST is a DNS name or unbracketed IP, not an SSH config alias. This first backend\n\
does not read ~/.ssh/config. Supply connection settings explicitly.\n\
Host keys must already be trusted in the supplied OpenSSH known_hosts file.\n\
Unknown/changed keys are never accepted automatically. Marker/pattern policies\n\
not supported by this backend are rejected, not ignored.\n\
--identity may be repeated; files are offered in order. --agent also offers\n\
keys from the local SSH agent. At least one of --identity or --agent is required.\n\
Agent-held sk-ssh-ed25519 keys are offered; sk-ecdsa is not.\n\
Defaults: port 22, history 200 lines (max 1000), timeout 10 seconds per operation.\n\
OS DNS resolution and local agent IPC are not covered by the network timeout.\n\
Default captures are NOT an atomic/interactive session.\n\
--watch restores pane models and collects\n\
read-only live output for 1..=3600 seconds, then prints their final screens.\n\
Some terminal parser state is not exposed by tmux; see docs/SYNCHRONIZATION.md.\n";

fn main() -> process::ExitCode {
    match run() {
        Ok(()) => process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("starcom-inspect: {}", format!("{error:#}").escape_debug());
            process::ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = env::args_os().skip(1);
    let mut values = collections::BTreeMap::<String, ffi::OsString>::new();
    let mut agent = false;
    let mut identities = Vec::new();
    while let Some(arg) = args.next() {
        let arg = arg.to_str().context("option must be UTF-8")?;
        match arg {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--agent" => {
                anyhow::ensure!(!agent, "--agent supplied twice");
                agent = true;
            }
            "--identity" => {
                identities.push(path::PathBuf::from(
                    args.next().context("--identity needs a file")?,
                ));
            }
            "--host" | "--user" | "--session" | "--known-hosts" | "--port" | "--socket"
            | "--history" | "--timeout" | "--watch" => {
                let value = args
                    .next()
                    .with_context(|| format!("{arg} needs a value"))?;
                anyhow::ensure!(
                    values.insert(arg.to_owned(), value).is_none(),
                    "duplicate option {arg}"
                );
            }
            _ => anyhow::bail!("unknown option {arg:?}; use --help"),
        }
    }
    if values.is_empty() && !agent && identities.is_empty() {
        print!("{HELP}");
        return Ok(());
    }
    let host = text(&mut values, "--host")?;
    let user = text(&mut values, "--user")?;
    let session = core::SessionName::new(text(&mut values, "--session")?)?;
    let known_hosts = path::PathBuf::from(
        values
            .remove("--known-hosts")
            .context("--known-hosts is required")?,
    );
    anyhow::ensure!(
        agent || !identities.is_empty(),
        "supply --identity FILE and/or --agent"
    );
    let authentication = ssh::Authentication {
        files: identities,
        agent,
        identities_only: false,
    };
    let port = optional_number(&mut values, "--port", 22)?;
    let history = optional_number(&mut values, "--history", 200)?;
    let seconds: u64 = optional_number(&mut values, "--timeout", 10)?;
    let socket = values
        .remove("--socket")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("remote socket path must be UTF-8"))
        })
        .transpose()?;
    anyhow::ensure!(
        history <= inspect::MAX_HISTORY_LINES,
        "--history exceeds {}",
        inspect::MAX_HISTORY_LINES
    );
    let watch_seconds = if values.contains_key("--watch") {
        let seconds: u64 = optional_number(&mut values, "--watch", 0)?;
        anyhow::ensure!(
            (1..=3600).contains(&seconds),
            "--watch must be 1..=3600 seconds"
        );
        Some(seconds)
    } else {
        None
    };
    let options = ssh::Options {
        host,
        port,
        user,
        known_hosts,
        authentication,
        host_key_alias: None,
        timeout: time::Duration::from_secs(seconds),
        jumps: Vec::new(),
    };
    if let Some(seconds) = watch_seconds {
        return watch(&options, &session, socket.as_deref(), history, seconds);
    }
    let mut inspector = inspect::Inspector::attach(&options, &session, socket.as_deref())?;
    let observation = inspector.observe(history)?;
    println!(
        "Starcom inspection: tmux {:?}, session {}, {} panes, verified {}",
        observation.tmux_version,
        observation.session,
        observation.captures.len(),
        observation.fingerprint
    );
    println!("Read-only observations; not an atomic snapshot or an interactive/restored terminal.");
    if observation.metadata_changed_during_capture {
        println!("Warning: pane metadata changed during capture.");
    }
    for capture in observation.captures {
        println!(
            "Pane {} window {}: {}x{} at {},{}; cursor {},{}; alternate={}; retained history={}/{}",
            capture.pane.id,
            capture.pane.window,
            capture.pane.size.columns(),
            capture.pane.size.rows(),
            capture.pane.left,
            capture.pane.top,
            capture.pane.cursor_x,
            capture.pane.cursor_y,
            capture.pane.alternate_screen,
            capture.pane.history_size,
            capture.pane.history_limit
        );
        for (row, text) in capture.escaped_rows.iter().enumerate() {
            if !text.is_empty() {
                println!("  {row:>4}: {text:?}");
            }
        }
    }
    Ok(())
}

fn watch(
    options: &ssh::Options,
    name: &core::SessionName,
    socket: Option<&str>,
    history: usize,
    seconds: u64,
) -> anyhow::Result<()> {
    let mut session = session::Session::attach(options, name, socket, history)?;
    println!(
        "Read-only live models: session {}, {} panes; collecting {seconds} seconds of output.",
        session.view().session,
        session.view().panes().len()
    );
    println!("Reconstructed observable state; not a complete tmux parser checkpoint.");
    let deadline = time::Instant::now() + time::Duration::from_secs(seconds);
    while time::Instant::now() < deadline {
        session.poll(deadline.min(time::Instant::now() + time::Duration::from_millis(250)))?;
        match session.view().status() {
            snapshot::Status::Watching => {}
            snapshot::Status::NeedsResync => {
                println!("Resynchronizing: topology changed or output continuity was lost.");
                session.synchronize()?;
            }
            snapshot::Status::Disconnected => {
                anyhow::bail!("tmux detached during live observation")
            }
        }
    }
    for (id, pane) in session.view().panes() {
        let size = pane.terminal.size();
        println!(
            "Pane {id}: {}x{}; alternate={}",
            size.columns(),
            size.rows(),
            pane.terminal.is_alternate_screen()
        );
        if pane.history_may_be_truncated {
            println!("  History is bounded; earlier retained lines may not be loaded.");
        }
        for (row, text) in pane.terminal.screen_lines().iter().enumerate() {
            if !text.is_empty() {
                println!("  {row:>4}: {text:?}");
            }
        }
    }
    Ok(())
}

fn text(
    values: &mut collections::BTreeMap<String, ffi::OsString>,
    key: &str,
) -> anyhow::Result<String> {
    values
        .remove(key)
        .with_context(|| format!("{key} is required"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{key} must be UTF-8"))
}

fn optional_number<T: std::str::FromStr>(
    values: &mut collections::BTreeMap<String, ffi::OsString>,
    key: &str,
    default: T,
) -> anyhow::Result<T> {
    match values.remove(key) {
        Some(value) => value
            .to_str()
            .with_context(|| format!("{key} must be UTF-8"))?
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid number for {key}")),
        None => Ok(default),
    }
}
