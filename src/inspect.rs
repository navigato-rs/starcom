//! Bounded tmux inspection and explicitly authorized interactive requests.
//!
//! `observe` produces independent observations, not an atomic terminal snapshot.
//! The session module coordinates a snapshot-to-live boundary over this channel.
//! Readiness is established by a complete snapshot, not just an SSH handshake.

use std::{collections, io, time};

use crate::{command, control, core, input, session, snapshot, ssh};
use anyhow::Context;

const MAX_PANES: usize = 128;
const MAX_STDERR: usize = 64 * 1024;
const MAX_TRANSFER: usize = 8 * 1024 * 1024;
pub const MAX_HISTORY_LINES: usize = 1000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    pub id: tmuxctl::PaneId,
    pub window: tmuxctl::WindowId,
    pub size: core::Size,
    pub left: usize,
    pub top: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub alternate_screen: bool,
    pub history_size: usize,
    pub history_limit: usize,
}

#[derive(Debug)]
pub struct Capture {
    pub pane: Pane,
    /// Plain text in tmux's -C escaped representation, not VT playback bytes.
    pub escaped_rows: Vec<String>,
}

#[derive(Debug)]
pub struct Observation {
    pub tmux_version: String,
    pub session: tmuxctl::SessionId,
    pub fingerprint: String,
    pub captures: Vec<Capture>,
    /// A useful warning, NOT proof of atomicity when false. Output may change
    /// without changing topology/cursor/history metadata.
    pub metadata_changed_during_capture: bool,
}

pub(crate) struct Batch {
    pub replies: Vec<Vec<String>>,
    /// The number of completed replies at each notification's wire position.
    pub notifications: Vec<(usize, tmuxctl::Notification)>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReadPurpose {
    Reply,
    Poll,
}

pub(crate) struct Interaction {
    pub applied: bool,
    pub notifications: Vec<tmuxctl::Notification>,
}

#[cfg(feature = "gui")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputLatency {
    pub queue: time::Duration,
    pub control: time::Duration,
    pub output_bytes: usize,
    pub output_events: usize,
    pub max_output_lag_ms: u64,
}

#[cfg(feature = "gui")]
pub(crate) struct InputCompletion {
    pub applied: bool,
    pub latency: InputLatency,
}

#[cfg(feature = "gui")]
pub(crate) struct InputProgress {
    pub completions: Vec<InputCompletion>,
    pub notifications: Vec<tmuxctl::Notification>,
    /// The SSH channel yielded bytes. A local wake returns false.
    pub received: bool,
}

#[cfg(feature = "gui")]
struct PendingInput {
    ids: Vec<tmuxctl::CommandId>,
    replies: Vec<Vec<String>>,
    sent_at: time::Instant,
    deadline: time::Instant,
    queue: time::Duration,
    wire_bytes: usize,
    output_bytes: usize,
    output_events: usize,
    max_output_lag_ms: u64,
}

#[cfg(feature = "gui")]
pub(crate) struct PaneMove {
    pub applied: bool,
    pub session: core::SessionName,
    pub notifications: Vec<tmuxctl::Notification>,
}

pub struct Inspector {
    channel: ssh::Channel,
    control: control::Control,
    stderr: Vec<u8>,
    input_buffer_prefix: Option<String>,
    input_sequence: u64,
    #[cfg(feature = "gui")]
    pending_input: collections::VecDeque<PendingInput>,
    /// Set once tmux has answered anything at all. Until then, an ending control
    /// session means the attach never happened, which is a different failure
    /// from a session that ended later.
    answered: bool,
    /// Round-trip of the last small control command. Not a probe.
    pub last_rtt: Option<time::Duration>,
}

impl Inspector {
    pub fn attach(
        options: &ssh::Options,
        session: &core::SessionName,
        socket: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::attach_with_access(options, session, socket, session::Access::ReadOnly)
    }

    pub(crate) fn attach_with_access(
        options: &ssh::Options,
        session: &core::SessionName,
        socket: Option<&str>,
        access: session::Access,
    ) -> anyhow::Result<Self> {
        let command = attach_command_with_access(session, socket, access)?;
        let channel = ssh::Connection::connect(options)?.exec(&command)?;
        Ok(Self {
            channel,
            control: control::Control::new(control::Limits {
                pending_commands: 256,
                ..control::Limits::default()
            })?,
            stderr: Vec::new(),
            input_buffer_prefix: None,
            input_sequence: 0,
            #[cfg(feature = "gui")]
            pending_input: collections::VecDeque::new(),
            answered: false,
            last_rtt: None,
        })
    }

    /// Perform bounded, read-only queries. Each request has a deadline and every
    /// failure aborts the control stream. No command is ever automatically retried.
    pub fn observe(&mut self, history_lines: usize) -> anyhow::Result<Observation> {
        anyhow::ensure!(
            history_lines <= MAX_HISTORY_LINES,
            "history request exceeds {MAX_HISTORY_LINES} lines"
        );
        let info = self.request("display-message -p '#{version}|#{session_id}|#{client_control_mode}|#{client_readonly}|#{client_flags}|#{client_tty}'\n")?;
        let (version, session) = parse_info(&info)?;
        let before = self.panes()?;
        let mut captures = Vec::new();
        let mut total = 0usize;
        for pane in &before {
            let command = format!(
                "capture-pane -p -C -t {} -S -{} -E -\n",
                pane.id, history_lines
            );
            let rows = self.request(&command)?;
            for row in &rows {
                total = total
                    .checked_add(row.len())
                    .context("capture size overflow")?;
                anyhow::ensure!(total <= MAX_TRANSFER, "session capture exceeds 8 MiB");
            }
            captures.push(Capture {
                pane: pane.clone(),
                escaped_rows: rows,
            });
        }
        let after = self.panes()?;
        Ok(Observation {
            tmux_version: version,
            session,
            fingerprint: self.channel.fingerprint().to_owned(),
            captures,
            metadata_changed_during_capture: before != after,
        })
    }

    pub(crate) fn panes(&mut self) -> anyhow::Result<Vec<Pane>> {
        let lines = self.request(concat!(
            "list-panes -s -F '",
            "#{pane_id} #{window_id} #{pane_width} #{pane_height} ",
            "#{pane_left} #{pane_top} #{cursor_x} #{cursor_y} ",
            "#{alternate_on} #{history_size} #{history_limit}'\n"
        ))?;
        parse_panes(&lines)
    }

    /// Identify the attached server and session.
    ///
    /// A session id is NOT sufficient on its own: a freshly started tmux server
    /// numbers its first session `$0` again, so a replaced server would look
    /// like the same session. The server's pid and the session's creation time
    /// are what actually distinguish a replacement from a continuation.
    #[cfg(feature = "gui")]
    pub(crate) fn identity(&mut self) -> anyhow::Result<Identity> {
        let reply =
            self.request("display-message -p '#{pid}|#{session_id}|#{session_created}'\n")?;
        anyhow::ensure!(reply.len() == 1, "invalid tmux identity reply");
        let fields: Vec<_> = reply[0].split('|').collect();
        anyhow::ensure!(fields.len() == 3, "invalid tmux identity reply");
        Ok(Identity {
            server: fields[0].parse().context("invalid tmux server pid")?,
            session: tmuxctl::SessionId(
                fields[1]
                    .strip_prefix('$')
                    .context("invalid session id")?
                    .parse()?,
            ),
            created: fields[2].parse().context("invalid session creation time")?,
        })
    }

    pub(crate) fn request(&mut self, command: &str) -> anyhow::Result<Vec<String>> {
        let mut batch = self.request_batch(&[command.to_owned()])?;
        Ok(batch.replies.remove(0))
    }

    /// Tell tmux this client's cell size so pane widths match the GUI font.
    /// Returns notifications that arrived with the reply (layout-change and a
    /// TUI's SIGWINCH redraw). Callers must not drop them: restore starts with
    /// `no-output`, so a redraw that finishes after capture never reaches us.
    #[cfg(feature = "gui")]
    pub(crate) fn set_client_size(
        &mut self,
        size: core::Size,
    ) -> anyhow::Result<Vec<tmuxctl::Notification>> {
        let batch =
            self.request_batch(&[command::Command::client_size(size).as_str().to_owned()])?;
        Ok(batch
            .notifications
            .into_iter()
            .map(|(_, event)| event)
            .collect())
    }

    /// Deadline for waiting on a SIGWINCH redraw before `no-output` capture.
    #[cfg(feature = "gui")]
    pub(crate) fn resize_drain_until(&self) -> time::Instant {
        let wait = self
            .last_rtt
            .unwrap_or(time::Duration::from_millis(40))
            .saturating_mul(3)
            .clamp(
                time::Duration::from_millis(50),
                time::Duration::from_millis(200),
            );
        time::Instant::now() + wait
    }

    /// Read until the control stream is quiet or `until`. After a resize,
    /// SIGWINCH redraws must finish while output is still enabled.
    #[cfg(feature = "gui")]
    pub(crate) fn drain_notifications(
        &mut self,
        until: time::Instant,
    ) -> anyhow::Result<Vec<tmuxctl::Notification>> {
        let mut out = Vec::new();
        loop {
            let now = time::Instant::now();
            if now >= until {
                break;
            }
            let notes = self.poll(until)?;
            if notes.is_empty() {
                break;
            }
            out.extend(notes);
        }
        Ok(out)
    }

    /// Rename on the existing control stream. If tmux applies the command but
    /// its final reply is lost, abort this stream so the caller can reconnect
    /// to the optimistically saved name without replaying the command.
    #[cfg(feature = "gui")]
    pub(crate) fn rename_session(
        &mut self,
        session: tmuxctl::SessionId,
        name: &core::SessionName,
    ) -> anyhow::Result<Vec<tmuxctl::Notification>> {
        let command = command::Command::rename_session(session, name);
        match self.exchange_with_timeout(command.as_str(), 1, time::Duration::from_secs(5)) {
            Ok(batch) => Ok(batch
                .notifications
                .into_iter()
                .map(|(_, event)| event)
                .collect()),
            Err(error) => {
                if error.to_string().contains("tmux rejected request") {
                    return Err(error);
                }
                self.abort();
                Err(ssh::Error::transport(
                    "tmux rename completion was not observed; the command was not retried",
                )
                .into())
            }
        }
    }

    /// Move one pane into a fresh detached session on this same tmux server.
    /// The temporary pane id is read from tmux, so this is independent of the
    /// user's base-index setting. A guarded rejection removes the empty target;
    /// an uncertain transport result is never retried or cleaned up blindly.
    #[cfg(feature = "gui")]
    pub(crate) fn move_to_new_session(
        &mut self,
        target: input::Target,
        size: core::Size,
    ) -> anyhow::Result<PaneMove> {
        anyhow::ensure!(self.input_buffer_prefix.is_some(), "read-only attachment");
        let listed = self.request("list-sessions -F '#{session_name}'\n")?;
        let mut existing: collections::BTreeSet<_> = listed.into_iter().collect();
        let mut attempts = 0_u8;
        let (name, created) = loop {
            let name = move_session_name(&existing, target.pane)?;
            let command = command::Command::new_detached_session(&name, size);
            match self.exchange(command.as_str(), 1) {
                Ok(batch) => break (name, batch.replies.into_iter().next().unwrap_or_default()),
                Err(error) if error.to_string().contains("tmux rejected request") => {
                    // A race can claim the name after the listing. `%error`
                    // completed the reply, so it is safe to ask which case it
                    // was and choose another name only when that name exists.
                    let current: collections::BTreeSet<_> = self
                        .request("list-sessions -F '#{session_name}'\n")?
                        .into_iter()
                        .collect();
                    if !current.contains(name.as_str()) {
                        return Err(error).context("could not create pane-move session");
                    }
                    existing = current;
                    attempts += 1;
                    anyhow::ensure!(attempts < 8, "session names kept changing during pane move");
                }
                Err(error) => {
                    self.abort();
                    return Err(error).context("could not create pane-move session");
                }
            }
        };
        anyhow::ensure!(created.len() == 1, "invalid new-session reply");
        let placeholder = created[0]
            .strip_prefix('%')
            .context("invalid placeholder pane id")?
            .parse()
            .context("invalid placeholder pane id")?;
        let placeholder = tmuxctl::PaneId(placeholder);
        let commands = [
            command::Command::join_pane(target.pane, placeholder),
            command::Command::kill_pane(placeholder),
        ];
        let mut wire = format!(
            "if-shell -F -t {} '{}' {{ ",
            target.pane,
            target.guard(true)
        );
        for command in &commands {
            wire.push_str(command.as_str().trim_end_matches('\n'));
            wire.push_str(" ; ");
        }
        wire.push_str("display-message -p STARCOM-APPLIED } { ");
        for _ in &commands {
            wire.push_str("display-message -p '' ; ");
        }
        wire.push_str("display-message -p STARCOM-BLOCKED }\n");
        let result = self.exchange(&wire, commands.len() + 2);
        if result.is_err() {
            self.abort();
        }
        let batch =
            result.context("pane move failed; delivery may be uncertain and was not retried")?;
        let last = batch
            .replies
            .last()
            .ok_or_else(|| anyhow::anyhow!("missing pane-move result"))?;
        anyhow::ensure!(
            last.len() == 1 && matches!(last[0].as_str(), "STARCOM-APPLIED" | "STARCOM-BLOCKED"),
            "invalid pane-move result"
        );
        let applied = last[0] == "STARCOM-APPLIED";
        let notifications = batch
            .notifications
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>();
        if !applied {
            let cleanup = self.request(command::Command::kill_session(&name).as_str())?;
            anyhow::ensure!(cleanup.is_empty(), "invalid move cleanup reply");
        }
        Ok(PaneMove {
            applied,
            session: name,
            notifications,
        })
    }

    /// One newline-terminated, synchronous tmux command list. Register every
    /// reply before writing; never retry a partial write or failed command list.
    pub(crate) fn request_batch(&mut self, commands: &[String]) -> anyhow::Result<Batch> {
        let result = self.request_batch_inner(commands);
        if result.is_err() {
            self.abort();
        }
        result.with_context(|| {
            let brief: String = String::from_utf8_lossy(&self.stderr)
                .chars()
                .take(512)
                .collect();
            format!("tmux request failed; remote stderr: {brief:?}")
        })
    }

    fn request_batch_inner(&mut self, commands: &[String]) -> anyhow::Result<Batch> {
        anyhow::ensure!(
            !commands.is_empty() && commands.len() <= 256,
            "invalid command count"
        );
        let mut wire = String::new();
        for command in commands {
            let command = command.strip_suffix('\n').unwrap_or(command);
            anyhow::ensure!(
                !command.is_empty()
                    && command.len() <= 4096
                    && !command.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0')),
                "invalid internal control command"
            );
            if !wire.is_empty() {
                wire.push_str(" ; ");
            }
            wire.push_str(command);
        }
        anyhow::ensure!(wire.len() < 512 * 1024, "command batch exceeds 512 KiB");
        wire.push('\n');
        self.exchange(&wire, commands.len())
    }

    /// `if-shell -F` emits a reply for itself and each chosen branch command.
    /// Both branches of an interactive transaction have the same reply count.
    fn exchange(&mut self, wire: &str, reply_count: usize) -> anyhow::Result<Batch> {
        self.exchange_with_timeout(wire, reply_count, self.channel.timeout())
    }

    fn exchange_with_timeout(
        &mut self,
        wire: &str,
        reply_count: usize,
        timeout: time::Duration,
    ) -> anyhow::Result<Batch> {
        anyhow::ensure!(reply_count > 0 && reply_count <= 256, "invalid reply count");
        anyhow::ensure!(
            wire.len() < 512 * 1024,
            "control transaction exceeds budget"
        );
        #[cfg(feature = "gui")]
        anyhow::ensure!(
            self.pending_input.is_empty(),
            "synchronous command attempted while input replies are pending"
        );
        let started = time::Instant::now();
        let deadline = started + self.channel.timeout().min(timeout);
        let ids = self.register_and_write(wire, reply_count, deadline)?;
        let mut batch = Batch {
            replies: Vec::new(),
            notifications: Vec::new(),
        };
        let mut total = 0usize;
        loop {
            let (events, bytes) = self
                .read_events(deadline, ReadPurpose::Reply)?
                .ok_or_else(|| ssh::Error::timeout("tmux reply deadline expired"))?;
            total = total.checked_add(bytes).context("wire budget overflow")?;
            anyhow::ensure!(
                total <= MAX_TRANSFER,
                "tmux batch exceeds 8 MiB wire budget"
            );
            for event in events {
                match event {
                    tmuxctl::Incoming::Reply { id, result } => {
                        anyhow::ensure!(
                            ids.get(batch.replies.len()) == Some(&id),
                            "unexpected command reply"
                        );
                        batch.replies.push(
                            result
                                .map_err(|error| {
                                    anyhow::anyhow!("tmux rejected request: {error:?}")
                                })?
                                .lines,
                        );
                        self.answered = true;
                    }
                    tmuxctl::Incoming::Notification(tmuxctl::Notification::Exit(reason)) => {
                        // Some successful commands deliberately remove the
                        // attached session (notably a one-pane handoff). Once
                        // every requested reply arrived, the transaction is
                        // certain and the caller must be allowed to publish it.
                        if batch.replies.len() == reply_count {
                            if reply_count <= 8 {
                                self.last_rtt = Some(started.elapsed());
                            }
                            return Ok(batch);
                        }
                        // tmux reports a failed attach inside the control stream
                        // (a %begin/%error block) and then exits, before any of
                        // our commands are outstanding, so those lines are not
                        // correlated to anything and are dropped. What is
                        // certain is that nothing was ever attached.
                        anyhow::ensure!(
                            self.answered,
                            "tmux could not attach that session; it may not exist on this server"
                        );
                        anyhow::bail!("tmux attachment ended ({reason:?})");
                    }
                    tmuxctl::Incoming::Notification(notification) => {
                        batch
                            .notifications
                            .push((batch.replies.len(), notification));
                    }
                }
            }
            if batch.replies.len() == reply_count {
                if reply_count <= 8 {
                    self.last_rtt = Some(started.elapsed());
                }
                return Ok(batch);
            }
        }
    }

    fn register_and_write(
        &mut self,
        wire: &str,
        reply_count: usize,
        deadline: time::Instant,
    ) -> anyhow::Result<Vec<tmuxctl::CommandId>> {
        anyhow::ensure!(reply_count > 0 && reply_count <= 256, "invalid reply count");
        anyhow::ensure!(
            wire.len() < 512 * 1024,
            "control transaction exceeds budget"
        );
        let mut ids = Vec::with_capacity(reply_count);
        for _ in 0..reply_count {
            ids.push(self.control.register_command()?);
        }
        let mut unsent = wire.as_bytes();
        while !unsent.is_empty() {
            if time::Instant::now() >= deadline {
                return Err(ssh::Error::timeout(
                    "tmux write deadline expired; delivery is uncertain",
                )
                .into());
            }
            match io::Write::write(&mut self.channel, unsent) {
                Ok(0) => {
                    return Err(ssh::Error::transport(
                        "tmux channel closed during write; delivery is uncertain",
                    )
                    .into());
                }
                Ok(count) => unsent = &unsent[count..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.channel.take_wakeup();
                    self.channel.wait(deadline)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(error).context("tmux write failed; delivery is uncertain");
                }
            }
        }
        Ok(ids)
    }

    /// Read at most one stdout/stderr chunk. None is an idle deadline, not EOF.
    /// The return value preserves wire order, including output after the last
    /// reply in the same SSH packet. Network chunk boundaries are not barriers.
    fn read_events(
        &mut self,
        deadline: time::Instant,
        purpose: ReadPurpose,
    ) -> anyhow::Result<Option<(Vec<tmuxctl::Incoming>, usize)>> {
        let mut buffer = [0; 8192];
        loop {
            let woken = self.channel.take_wakeup();
            if time::Instant::now() >= deadline {
                if purpose == ReadPurpose::Reply {
                    return Err(ssh::Error::timeout("tmux reply deadline expired").into());
                }
                return Ok(None);
            }
            if purpose == ReadPurpose::Poll && woken {
                return Ok(None);
            }
            let mut bytes = 0usize;
            match self.channel.read_stderr(&mut buffer) {
                Ok(count) => {
                    bytes += count;
                    anyhow::ensure!(
                        self.stderr.len() + count <= MAX_STDERR,
                        "remote stderr exceeds 64 KiB"
                    );
                    self.stderr.extend_from_slice(&buffer[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("read SSH stderr"),
            }
            let mut events = Vec::new();
            match io::Read::read(&mut self.channel, &mut buffer) {
                Ok(count) => {
                    bytes += count;
                    self.control
                        .feed(&buffer[..count], |event| events.push(event))?;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("read SSH stdout"),
            }
            for event in &events {
                if let tmuxctl::Incoming::Notification(tmuxctl::Notification::Unknown(ref text)) =
                    *event
                {
                    anyhow::ensure!(
                        text.starts_with('%'),
                        "unexpected stdout in tmux control stream"
                    );
                }
            }
            if bytes != 0 {
                return Ok(Some((events, bytes)));
            }
            // EOF without a preceding %exit means the channel died under us.
            // tmux announces an orderly end; reaching here does not.
            if self.channel.eof() {
                return Err(ssh::Error::transport(
                    "SSH control channel ended before tmux reported an exit",
                )
                .into());
            }
            if let Err(error) = self.channel.wait(deadline) {
                if error.kind == ssh::Kind::Timeout {
                    if purpose == ReadPurpose::Reply {
                        return Err(error.into());
                    }
                    return Ok(None);
                }
                return Err(error.into());
            }
        }
    }

    pub(crate) fn poll(
        &mut self,
        deadline: time::Instant,
    ) -> anyhow::Result<Vec<tmuxctl::Notification>> {
        let result = (|| {
            let Some((events, _)) = self.read_events(deadline, ReadPurpose::Poll)? else {
                return Ok(Vec::new());
            };
            let mut notifications = Vec::new();
            for event in events {
                match event {
                    tmuxctl::Incoming::Notification(notification) => {
                        notifications.push(notification)
                    }
                    tmuxctl::Incoming::Reply { .. } => anyhow::bail!("unsolicited command reply"),
                }
            }
            Ok(notifications)
        })();
        if result.is_err() {
            self.abort();
        }
        result
    }

    #[cfg(feature = "gui")]
    pub(crate) fn waker(&self) -> ssh::Wake {
        self.channel.waker()
    }

    /// Enable application input only after the initial snapshot and policy
    /// checks. tmux's read-only flag is sticky, so interactive connections attach
    /// without it; the application gate remains closed until this succeeds.
    /// Interactive attachments report their cell size; they do not use
    /// ignore-size, or pane math cannot match what the GUI paints.
    pub(crate) fn enable_input(
        &mut self,
        view: &snapshot::View,
    ) -> anyhow::Result<Vec<tmuxctl::Notification>> {
        let aliases = self.request_batch(&["show-options -s -v command-alias".to_owned()])?;
        for alias in &aliases.replies[0] {
            let name = alias.split_once('=').map(|(name, _)| name).unwrap_or("");
            anyhow::ensure!(
                !matches!(
                    name,
                    "send-keys"
                        | "set-buffer"
                        | "paste-buffer"
                        | "resize-pane"
                        | "split-window"
                        | "kill-pane"
                        | "select-pane"
                        | "swap-pane"
                        | "rename-session"
                        | "new-session"
                        | "join-pane"
                        | "kill-session"
                        | "list-sessions"
                        | "if-shell"
                        | "display-message"
                ),
                "a command alias overrides an interactive command"
            );
        }
        let mut hooks = vec![
            "show-hooks -g".to_owned(),
            "show-hooks -gw".to_owned(),
            format!("show-hooks -t '{}'", view.session),
        ];
        let windows: collections::BTreeSet<_> = view
            .panes()
            .values()
            .map(|pane| pane.state.window)
            .collect();
        hooks.extend(
            windows
                .iter()
                .map(|window| format!("show-hooks -w -t {window}")),
        );
        hooks.extend(
            view.panes()
                .keys()
                .map(|pane| format!("show-hooks -p -t {pane}")),
        );
        let hooks = self.request_batch(&hooks)?;
        for lines in &hooks.replies {
            for line in lines {
                let hook = line
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .split('[')
                    .next()
                    .unwrap_or("");
                if matches!(
                    hook,
                    "after-send-keys"
                        | "after-set-buffer"
                        | "after-paste-buffer"
                        | "after-resize-pane"
                        | "after-split-window"
                        | "after-kill-pane"
                        | "after-select-pane"
                        | "after-swap-pane"
                        | "after-rename-session"
                        | "after-new-session"
                        | "after-join-pane"
                        | "after-kill-session"
                        | "after-if-shell"
                        | "after-display-message"
                ) {
                    anyhow::ensure!(
                        line == hook,
                        "configured {hook} hook prevents safe interactive transactions"
                    );
                }
            }
        }
        let batch = self.request_batch(&[
            "display-message -p '#{client_control_mode}|#{client_readonly}|#{client_flags}|#{client_tty}|#{client_pid}'".to_owned(),
        ])?;
        anyhow::ensure!(
            batch.replies[0].len() == 1,
            "invalid interactive client metadata"
        );
        let fields: Vec<_> = batch.replies[0][0].split('|').collect();
        anyhow::ensure!(fields.len() == 5, "invalid interactive client metadata");
        // Snapshot already ran refresh-client -f '!no-output' so live bytes
        // can flow. Do not require no-output here.
        let flags: collections::BTreeSet<_> = fields[2].split(',').collect();
        anyhow::ensure!(fields[0] == "1", "tmux dropped control mode");
        anyhow::ensure!(fields[1] == "0", "tmux made the client read-only");
        anyhow::ensure!(
            !flags.contains("ignore-size"),
            "tmux still ignores this client's size"
        );
        anyhow::ensure!(fields[3].is_empty(), "tmux allocated a PTY");
        let pid: u32 = fields[4].parse()?;
        let stamp = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)?
            .as_nanos();
        self.input_buffer_prefix = Some(format!("starcom-{pid}-{stamp:x}"));
        Ok(aliases
            .notifications
            .into_iter()
            .chain(hooks.notifications)
            .chain(batch.notifications)
            .map(|(_, event)| event)
            .collect())
    }

    fn interaction_wire(
        &mut self,
        target: input::Target,
        actions: &[input::Action],
    ) -> anyhow::Result<(String, usize)> {
        anyhow::ensure!(
            !actions.is_empty() && actions.len() <= 32,
            "invalid input batch"
        );
        let prefix = self
            .input_buffer_prefix
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("read-only attachment"))?;
        self.input_sequence = self
            .input_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("input sequence exhausted"))?;
        let mut commands = Vec::new();
        let resizing = actions
            .iter()
            .any(|action| matches!(action, input::Action::Resize(_)));
        for action in actions {
            action.validate()?;
            match *action {
                input::Action::Bytes(ref bytes) => {
                    commands.push(command::Command::send_bytes(target.pane, bytes)?)
                }
                input::Action::Key(key, modifiers) => {
                    commands.push(command::Command::send_key(target.pane, key, modifiers)?)
                }
                input::Action::Resize(resize) => {
                    anyhow::ensure!(actions.len() == 1, "resize must be a separate transaction");
                    commands.push(command::Command::resize_axis(target.pane, resize)?);
                }
                input::Action::Split(axis) => {
                    anyhow::ensure!(actions.len() == 1, "split must be a separate transaction");
                    commands.push(command::Command::split_pane(target.pane, axis));
                }
                input::Action::KillPane => {
                    anyhow::ensure!(
                        actions.len() == 1,
                        "kill-pane must be a separate transaction"
                    );
                    commands.push(command::Command::kill_pane(target.pane));
                }
                input::Action::ZoomPane => {
                    anyhow::ensure!(actions.len() == 1, "zoom must be a separate transaction");
                    commands.push(command::Command::zoom_pane(target.pane));
                }
                input::Action::SelectPane => {
                    anyhow::ensure!(
                        actions.len() == 1,
                        "select-pane must be a separate transaction"
                    );
                    commands.push(command::Command::select_pane(target.pane));
                }
                input::Action::MoveToNewSession => {
                    anyhow::bail!("pane moves use their own transaction")
                }
                input::Action::SwapPane(other) => {
                    anyhow::ensure!(
                        actions.len() == 1,
                        "swap-pane must be a separate transaction"
                    );
                    anyhow::ensure!(other != target.pane, "cannot swap a pane with itself");
                    commands.push(command::Command::swap_pane(target.pane, other));
                }
                input::Action::ClientSize(_) => {
                    anyhow::bail!("client size is not a pane transaction")
                }
                input::Action::Paste(ref paste) => {
                    anyhow::ensure!(actions.len() == 1, "paste must be a separate transaction");
                    commands.extend(command::Command::paste(
                        target.pane,
                        paste,
                        &format!("{prefix}-{}", self.input_sequence),
                    ));
                }
            }
        }
        // No shell is spawned. The condition is evaluated by tmux immediately
        // before inserting the synchronous action commands. Both branches end
        // with an explicit marker; guard rejection is not a transport failure.
        let mut wire = format!(
            "if-shell -F -t {} '{}' {{ ",
            target.pane,
            target.guard(resizing)
        );
        for command in &commands {
            wire.push_str(command.as_str().trim_end_matches('\n'));
            wire.push_str(" ; ");
        }
        wire.push_str("display-message -p STARCOM-APPLIED } { ");
        for _ in &commands {
            wire.push_str("display-message -p '' ; ");
        }
        wire.push_str("display-message -p STARCOM-BLOCKED }\n");
        Ok((wire, commands.len() + 2))
    }

    pub(crate) fn interact(
        &mut self,
        target: input::Target,
        actions: &[input::Action],
    ) -> anyhow::Result<Interaction> {
        let (wire, reply_count) = self.interaction_wire(target, actions)?;
        let result = self.exchange(&wire, reply_count);
        if result.is_err() {
            self.abort();
        }
        // Server errors may echo command arguments. Never surface input or
        // clipboard contents. Failure cannot be distinguished from delivery.
        //
        // Discard the message but keep our own transport classification: without
        // it, a channel that died mid-transaction looks like an unrecognized
        // protocol fault and reconnection policy would refuse to retry it. The
        // dropped action itself is still never resubmitted.
        let batch = result.map_err(opaque_interaction_error)?;
        let last = batch
            .replies
            .last()
            .ok_or_else(|| anyhow::anyhow!("missing interactive result"))?;
        anyhow::ensure!(
            last.len() == 1 && matches!(last[0].as_str(), "STARCOM-APPLIED" | "STARCOM-BLOCKED"),
            "invalid interactive result"
        );
        Ok(Interaction {
            applied: last[0] == "STARCOM-APPLIED",
            notifications: batch
                .notifications
                .into_iter()
                .map(|(_, event)| event)
                .collect(),
        })
    }

    #[cfg(feature = "gui")]
    pub(crate) fn input_idle(&self) -> bool {
        self.pending_input.is_empty()
    }

    #[cfg(feature = "gui")]
    pub(crate) fn can_pipeline_input(&self) -> bool {
        // One ordinary transaction has at most 32 actions plus `if-shell` and
        // its result marker. Keep a second bound below Control's hard cap so a
        // burst cannot monopolize the stream indefinitely.
        self.pending_input.len() < 8 && self.control.pending_commands() <= 222
    }

    /// Send ordinary key/byte input without waiting for earlier acknowledgments.
    /// Every transaction remains guarded and ordered by tmux. Once registered,
    /// it is never replayed, including after an uncertain write or disconnect.
    #[cfg(feature = "gui")]
    pub(crate) fn start_input(
        &mut self,
        target: input::Target,
        actions: &[input::Action],
        queue: time::Duration,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            actions
                .iter()
                .all(|action| matches!(action, input::Action::Bytes(_) | input::Action::Key(..))),
            "only ordinary terminal input may be pipelined"
        );
        anyhow::ensure!(self.can_pipeline_input(), "input pipeline is full");
        let (wire, reply_count) = self.interaction_wire(target, actions)?;
        let sent_at = time::Instant::now();
        let deadline = sent_at + self.channel.timeout();
        match self.register_and_write(&wire, reply_count, deadline) {
            Ok(ids) => {
                self.pending_input.push_back(PendingInput {
                    ids,
                    replies: Vec::with_capacity(reply_count),
                    sent_at,
                    deadline,
                    queue,
                    wire_bytes: 0,
                    output_bytes: 0,
                    output_events: 0,
                    max_output_lag_ms: 0,
                });
                Ok(())
            }
            Err(error) => {
                self.abort();
                Err(opaque_interaction_error(error))
            }
        }
    }

    /// Process at most one SSH chunk, or return when newly queued local input
    /// wakes the worker. This lets the caller add more guarded transactions
    /// while earlier replies are still in flight.
    #[cfg(feature = "gui")]
    pub(crate) fn poll_input(&mut self) -> anyhow::Result<InputProgress> {
        let result = self.poll_input_inner();
        if result.is_err() {
            self.abort();
        }
        result.map_err(opaque_interaction_error)
    }

    #[cfg(feature = "gui")]
    fn poll_input_inner(&mut self) -> anyhow::Result<InputProgress> {
        let deadline = self
            .pending_input
            .front()
            .context("input pipeline is empty")?
            .deadline;
        let Some((events, bytes)) = self.read_events(deadline, ReadPurpose::Poll)? else {
            anyhow::ensure!(
                time::Instant::now() < deadline,
                ssh::Error::timeout("tmux input reply deadline expired")
            );
            return Ok(InputProgress {
                completions: Vec::new(),
                notifications: Vec::new(),
                received: false,
            });
        };
        for pending in &mut self.pending_input {
            pending.wire_bytes = pending
                .wire_bytes
                .checked_add(bytes)
                .context("wire budget overflow")?;
            anyhow::ensure!(
                pending.wire_bytes <= MAX_TRANSFER,
                "tmux input pipeline exceeds 8 MiB wire budget"
            );
        }

        let mut progress = InputProgress {
            completions: Vec::new(),
            notifications: Vec::new(),
            received: true,
        };
        for event in events {
            match event {
                tmuxctl::Incoming::Notification(notification) => {
                    let (output_bytes, lag) = notification_output(&notification);
                    if output_bytes != 0 {
                        for pending in &mut self.pending_input {
                            pending.output_bytes = pending
                                .output_bytes
                                .checked_add(output_bytes)
                                .context("output byte count overflow")?;
                            pending.output_events += 1;
                            pending.max_output_lag_ms = pending.max_output_lag_ms.max(lag);
                        }
                    }
                    progress.notifications.push(notification);
                }
                tmuxctl::Incoming::Reply { id, result } => {
                    let complete = {
                        let pending = self
                            .pending_input
                            .front_mut()
                            .context("unexpected pipelined input reply")?;
                        anyhow::ensure!(
                            pending.ids.get(pending.replies.len()) == Some(&id),
                            "unexpected pipelined input reply"
                        );
                        pending.replies.push(
                            result
                                .map_err(|_| anyhow::anyhow!("tmux rejected pipelined input"))?
                                .lines,
                        );
                        self.answered = true;
                        pending.replies.len() == pending.ids.len()
                    };
                    if complete {
                        let pending = self.pending_input.pop_front().expect("front checked");
                        let last = pending
                            .replies
                            .last()
                            .context("missing pipelined input result")?;
                        anyhow::ensure!(
                            last.len() == 1
                                && matches!(
                                    last[0].as_str(),
                                    "STARCOM-APPLIED" | "STARCOM-BLOCKED"
                                ),
                            "invalid pipelined input result"
                        );
                        let control = pending.sent_at.elapsed();
                        self.last_rtt = Some(control);
                        progress.completions.push(InputCompletion {
                            applied: last[0] == "STARCOM-APPLIED",
                            latency: InputLatency {
                                queue: pending.queue,
                                control,
                                output_bytes: pending.output_bytes,
                                output_events: pending.output_events,
                                max_output_lag_ms: pending.max_output_lag_ms,
                            },
                        });
                    }
                }
            }
        }
        Ok(progress)
    }

    pub(crate) fn abort(&mut self) {
        let _ = self.control.finish(|_| {});
        self.channel.abort();
        #[cfg(feature = "gui")]
        self.pending_input.clear();
    }
}

#[cfg(feature = "gui")]
fn notification_output(notification: &tmuxctl::Notification) -> (usize, u64) {
    match *notification {
        tmuxctl::Notification::Output { ref bytes, .. } => (bytes.len(), 0),
        tmuxctl::Notification::ExtendedOutput {
            ref bytes,
            ms_behind,
            ..
        } => (bytes.len(), ms_behind),
        _ => (0, 0),
    }
}

fn opaque_interaction_error(error: anyhow::Error) -> anyhow::Error {
    const OPAQUE: &str =
        "interactive request failed; delivery may be uncertain and was not retried";
    if crate::reconnect::classify(&error) == crate::reconnect::Failure::Transport {
        anyhow::Error::new(ssh::Error::transport(OPAQUE))
    } else {
        anyhow::anyhow!(OPAQUE)
    }
}

impl Drop for Inspector {
    fn drop(&mut self) {
        let _ = self.control.finish(|_| {});
        self.channel.abort();
    }
}

#[cfg(feature = "gui")]
fn move_session_name(
    existing: &collections::BTreeSet<String>,
    pane: tmuxctl::PaneId,
) -> anyhow::Result<core::SessionName> {
    let stem = format!("pane-{}", pane.0);
    if !existing.contains(&stem) {
        return Ok(core::SessionName::new(stem)?);
    }
    for suffix in 2_u32.. {
        let candidate = format!("{stem}-{suffix}");
        if !existing.contains(&candidate) {
            return Ok(core::SessionName::new(candidate)?);
        }
    }
    unreachable!("u32 iterator is unbounded for practical session counts")
}

/// What makes one attachment the same session as the last one.
///
/// Only the desktop reconnects, so this is dead weight in a build without it.
#[cfg(feature = "gui")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    pub server: u32,
    pub session: tmuxctl::SessionId,
    pub created: u64,
}

#[cfg(test)]
fn attach_command(session: &core::SessionName, socket: Option<&str>) -> anyhow::Result<String> {
    attach_command_with_access(session, socket, session::Access::ReadOnly)
}

fn attach_command_with_access(
    session: &core::SessionName,
    socket: Option<&str>,
    access: session::Access,
) -> anyhow::Result<String> {
    let mut command = "exec tmux -N -C".to_owned();
    if let Some(socket) = socket {
        anyhow::ensure!(
            !socket.is_empty() && socket.len() <= 4096 && !socket.chars().any(char::is_control),
            "invalid tmux socket path"
        );
        command.push_str(" -S ");
        command.push_str(&command::shell_quote(socket)?);
    }
    // -N forbids starting a server. -E leaves session environment untouched.
    // read-only is a UI safety flag, not an authorization sandbox for commands.
    command.push_str(" attach-session -E -f ");
    if access == session::Access::ReadOnly {
        // Read-only inspection must not change the shared layout.
        command.push_str("read-only,ignore-size,no-output -t ");
    } else {
        // Interactive clients report their size; ignore-size would make the
        // painted column count disagree with tmux.
        command.push_str("no-output -t ");
    }
    command.push_str(&command::shell_quote(&format!("={}", session.as_str()))?);
    Ok(command)
}

pub(crate) fn parse_info(lines: &[String]) -> anyhow::Result<(String, tmuxctl::SessionId)> {
    parse_info_with_access(lines, session::Access::ReadOnly)
}

pub(crate) fn parse_info_with_access(
    lines: &[String],
    access: session::Access,
) -> anyhow::Result<(String, tmuxctl::SessionId)> {
    anyhow::ensure!(lines.len() == 1, "invalid tmux client metadata");
    let fields: Vec<_> = lines[0].split('|').collect();
    anyhow::ensure!(
        fields.len() == 6 && fields[0].len() <= 64 && !fields[0].is_empty(),
        "tmux does not provide the required client metadata"
    );
    anyhow::ensure!(
        fields[2] == "1"
            && fields[3]
                == if access == session::Access::ReadOnly {
                    "1"
                } else {
                    "0"
                }
            && fields[5].is_empty(),
        "unexpected control-client access mode or PTY"
    );
    let flags: collections::BTreeSet<_> = fields[4].split(',').collect();
    anyhow::ensure!(
        flags.contains("no-output")
            && flags.contains("ignore-size") == (access == session::Access::ReadOnly),
        "tmux did not apply the inspection safety flags"
    );
    let session = fields[1]
        .strip_prefix('$')
        .context("invalid tmux session id")?
        .parse()?;
    Ok((fields[0].to_owned(), tmuxctl::SessionId(session)))
}

fn parse_panes(lines: &[String]) -> anyhow::Result<Vec<Pane>> {
    anyhow::ensure!(
        !lines.is_empty() && lines.len() <= MAX_PANES,
        "invalid pane count (maximum {MAX_PANES})"
    );
    let mut seen = collections::BTreeSet::new();
    let mut panes = Vec::new();
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        anyhow::ensure!(fields.len() == 11, "invalid pane metadata");
        let id = tmuxctl::PaneId(
            fields[0]
                .strip_prefix('%')
                .context("invalid pane id")?
                .parse()?,
        );
        anyhow::ensure!(seen.insert(id), "duplicate pane id");
        let window = tmuxctl::WindowId(
            fields[1]
                .strip_prefix('@')
                .context("invalid window id")?
                .parse()?,
        );
        let size = core::Size::new(fields[2].parse()?, fields[3].parse()?)?;
        let left = fields[4].parse()?;
        let top = fields[5].parse()?;
        let (cursor_x, cursor_y) = size.clamp_cursor(fields[6].parse()?, fields[7].parse()?);
        let alternate_screen = match fields[8] {
            "0" => false,
            "1" => true,
            _ => anyhow::bail!("invalid alternate-screen state"),
        };
        anyhow::ensure!(
            left <= 65535 && top <= 65535,
            "pane position exceeds budget"
        );
        panes.push(Pane {
            id,
            window,
            size,
            left,
            top,
            cursor_x,
            cursor_y,
            alternate_screen,
            history_size: fields[9].parse()?,
            history_limit: fields[10].parse()?,
        });
    }
    panes.sort_by_key(|pane| pane.id);
    Ok(panes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "gui")]
    #[test]
    fn pane_move_names_are_short_and_avoid_existing_sessions() {
        let existing = collections::BTreeSet::from([
            "pane-7".to_owned(),
            "pane-7-2".to_owned(),
            "unrelated".to_owned(),
        ]);
        assert_eq!(
            move_session_name(&existing, tmuxctl::PaneId(7))
                .unwrap()
                .as_str(),
            "pane-7-3"
        );
        assert_eq!(
            move_session_name(&existing, tmuxctl::PaneId(8))
                .unwrap()
                .as_str(),
            "pane-8"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn output_notifications_report_payload_and_tmux_lag() {
        assert_eq!(
            notification_output(&tmuxctl::Notification::Output {
                pane: tmuxctl::PaneId(3),
                bytes: b"redraw".to_vec(),
            }),
            (6, 0)
        );
        assert_eq!(
            notification_output(&tmuxctl::Notification::ExtendedOutput {
                pane: tmuxctl::PaneId(3),
                ms_behind: 417,
                bytes: b"frame".to_vec(),
            }),
            (5, 417)
        );
        assert_eq!(
            notification_output(&tmuxctl::Notification::SessionsChanged),
            (0, 0)
        );
    }

    #[test]
    fn attachment_does_not_create_resize_or_change_environment() {
        let session = core::SessionName::new("work's; $(false)").unwrap();
        assert_eq!(
            attach_command(&session, Some("/tmp/a'b")).unwrap(),
            "exec tmux -N -C -S '/tmp/a'\"'\"'b' attach-session -E -f read-only,ignore-size,no-output -t '=work'\"'\"'s; $(false)'"
        );
        assert_eq!(
            attach_command_with_access(&session, None, session::Access::Interactive).unwrap(),
            "exec tmux -N -C attach-session -E -f no-output -t '=work'\"'\"'s; $(false)'"
        );
        assert!(attach_command(&session, Some("x\nkill-server")).is_err());
    }

    #[test]
    fn client_capabilities_are_checked_not_assumed() {
        assert!(
            parse_info(&["3.3a|$0|1|1|control-mode,read-only,ignore-size,no-output|".to_owned()])
                .is_ok()
        );
        assert!(parse_info(&["3.3a|$0|1|0|ignore-size,no-output|".to_owned()]).is_err());
        assert!(parse_info(&["3.3a|$0|1|1|ignore-size|".to_owned()]).is_err());
        assert!(parse_info(&["3.3a|$0|1|1|ignore-size,no-output|/dev/pts/0".to_owned()]).is_err());
        assert!(
            parse_info_with_access(
                &["3.3a|$0|1|0|control-mode,no-output|".to_owned()],
                session::Access::Interactive,
            )
            .is_ok()
        );
        assert!(
            parse_info_with_access(
                &["3.3a|$0|1|0|control-mode,ignore-size,no-output|".to_owned()],
                session::Access::Interactive,
            )
            .is_err()
        );
    }

    #[test]
    fn metadata_is_validated_before_allocation() {
        let valid = "%0 @1 80 24 0 0 80 23 1 0 2000".to_owned();
        let panes = parse_panes(std::slice::from_ref(&valid)).unwrap();
        assert!(panes[0].alternate_screen);
        assert_eq!(panes[0].size, core::Size::default());
        assert!(parse_panes(&[valid.clone(), valid]).is_err());
        for line in [
            "%0 @0 999999 999999 0 0 0 0 0 0 0",
            "%0 @0 80 24 0 0 0 0 2 0 0",
            "pane @0 80 24 0 0 0 0 0 0 0",
        ] {
            assert!(parse_panes(&[line.to_owned()]).is_err());
        }
        let clamped = parse_panes(&["%0 @0 80 24 0 0 120 40 1 0 2000".to_owned()]).unwrap();
        assert_eq!(clamped[0].cursor_x, 80);
        assert_eq!(clamped[0].cursor_y, 23);
    }
}
