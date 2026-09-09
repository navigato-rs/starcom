//! Snapshot-to-live sessions with explicit read-only or interactive access.
//!
//! A failed restore aborts the channel and leaves the previous view readable.
//! User actions are never retried, and no automatic device replies are sent.

use std::{collections, time};

use crate::{core, input, inspect, snapshot, ssh};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    ReadOnly,
    Interactive,
}

pub struct Session {
    inspector: inspect::Inspector,
    view: snapshot::View,
    history: usize,
    access: Access,
}

impl Session {
    pub fn attach(
        options: &ssh::Options,
        session: &core::SessionName,
        socket: Option<&str>,
        history: usize,
    ) -> anyhow::Result<Self> {
        Self::attach_with_access(options, session, socket, history, Access::ReadOnly)
    }

    pub fn attach_with_access(
        options: &ssh::Options,
        session: &core::SessionName,
        socket: Option<&str>,
        history: usize,
        access: Access,
    ) -> anyhow::Result<Self> {
        let mut inspector =
            inspect::Inspector::attach_with_access(options, session, socket, access)?;
        let info = inspector.request("display-message -p '#{version}|#{session_id}|#{client_control_mode}|#{client_readonly}|#{client_flags}|#{client_tty}'\n")?;
        let (_, session) = inspect::parse_info_with_access(&info, access)?;
        let mut view = restore(&mut inspector, session, history)?;
        if access == Access::Interactive {
            for event in inspector.enable_input(&view)? {
                view.apply(event);
            }
        }
        Ok(Self {
            inspector,
            view,
            history,
            access,
        })
    }

    /// The desktop worker keeps I/O separate from the briefly locked models.
    #[cfg(feature = "gui")]
    pub(crate) fn into_parts(self) -> (inspect::Inspector, snapshot::View) {
        (self.inspector, self.view)
    }

    pub fn view(&self) -> &snapshot::View {
        &self.view
    }

    /// Service live output up to one bounded read. An idle deadline is normal.
    /// Call synchronize after NeedsResync; data is ignored while invalidated.
    pub fn poll(&mut self, deadline: time::Instant) -> anyhow::Result<()> {
        match self.inspector.poll(deadline) {
            Ok(notifications) => {
                for notification in notifications {
                    self.view.apply(notification);
                }
                Ok(())
            }
            Err(error) => {
                self.view.disconnect();
                Err(error)
            }
        }
    }

    /// Explicit caller-originated input only. Success acknowledges tmux's
    /// command, not application completion. Never retry on an uncertain result.
    pub fn interact(
        &mut self,
        pane: tmuxctl::PaneId,
        action: &input::Action,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.access == Access::Interactive, "read-only attachment");
        validate_action(&self.view, pane, action)?;
        let target = action_target(&self.view, pane)?;
        match self
            .inspector
            .interact(target, std::slice::from_ref(action))
        {
            Ok(outcome) => {
                for event in outcome.notifications {
                    self.view.apply(event);
                }
                if !outcome.applied {
                    self.view.invalidate();
                    anyhow::bail!(
                        "tmux blocked input: pane changed, is in a mode, or synchronize-panes is enabled"
                    );
                }
                if action.changes_layout() {
                    self.view.invalidate();
                }
                Ok(())
            }
            Err(error) => {
                self.view.disconnect();
                Err(error)
            }
        }
    }

    /// Replace all pane models, never append captures to their old buffers.
    /// No old model is published as current while the transaction is incomplete.
    pub fn synchronize(&mut self) -> anyhow::Result<()> {
        let result = (|| {
            let mut view = restore(&mut self.inspector, self.view.session, self.history)?;
            if self.access == Access::Interactive {
                for event in self.inspector.enable_input(&view)? {
                    view.apply(event);
                }
            }
            Ok(view)
        })();
        match result {
            Ok(mut view) => {
                view.preserve_history_offsets(&self.view);
                self.view = view;
                Ok(())
            }
            Err(error) => {
                self.inspector.abort();
                self.view.disconnect();
                Err(error)
            }
        }
    }
}

pub(crate) fn action_target(
    view: &snapshot::View,
    pane: tmuxctl::PaneId,
) -> anyhow::Result<input::Target> {
    let state = &view
        .panes()
        .get(&pane)
        .ok_or_else(|| anyhow::anyhow!("pane is no longer in this session"))?
        .state;
    Ok(input::Target {
        session: view.session,
        pane,
        window: state.window,
        size: state.size,
        left: state.left,
        top: state.top,
    })
}

pub(crate) fn validate_action(
    view: &snapshot::View,
    pane: tmuxctl::PaneId,
    action: &input::Action,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        view.status() == snapshot::Status::Watching,
        "input requires a synchronized live view"
    );
    let pane = view
        .panes()
        .get(&pane)
        .ok_or_else(|| anyhow::anyhow!("pane is no longer in this session"))?;
    action.validate()?;
    if let input::Action::ClientSize(_) = *action {
        return Ok(());
    }
    if let input::Action::Resize(resize) = *action {
        let size = pane.state.size;
        match resize.axis {
            input::Axis::Columns => core::Size::new(resize.cells, size.rows())?,
            input::Axis::Rows => core::Size::new(size.columns(), resize.cells)?,
        };
    }
    if let input::Action::SwapPane(other) = *action {
        anyhow::ensure!(other != pane.state.pane, "cannot swap a pane with itself");
        let dest = view
            .panes()
            .get(&other)
            .ok_or_else(|| anyhow::anyhow!("swap target is no longer in this session"))?;
        anyhow::ensure!(
            dest.state.window == pane.state.window,
            "swap is within one window"
        );
    }
    Ok(())
}

pub(crate) fn restore(
    inspector: &mut inspect::Inspector,
    session: tmuxctl::SessionId,
    history: usize,
) -> anyhow::Result<snapshot::View> {
    anyhow::ensure!(
        history <= snapshot::MAX_HISTORY_LINES,
        "history exceeds budget"
    );
    // Drain all previously ordered output before this reply. This changes only
    // this control client, not the remote programs or other attached clients.
    inspector.request("refresh-client -f no-output\n")?;
    let panes = inspector.panes()?;
    anyhow::ensure!(
        panes.len() <= snapshot::MAX_PANES,
        "too many panes for live snapshot"
    );
    verify_snapshot_environment(inspector, session, &panes)?;
    let ids: Vec<_> = panes.iter().map(|pane| pane.id).collect();
    let commands = snapshot_commands(&ids, history);
    let batch = inspector.request_batch(&commands)?;
    let mut states = Vec::new();
    for reply in batch.replies.iter().take(ids.len() * 4).step_by(4) {
        anyhow::ensure!(reply.len() == 1, "invalid pane-state reply");
        states.push(snapshot::State::parse(&reply[0])?);
    }
    for (state, id) in states.iter().zip(&ids) {
        anyhow::ensure!(state.pane == *id, "pane identity changed during snapshot");
    }
    snapshot::validate_budget(&states, history)?;
    let final_topology = &batch.replies[ids.len() * 4];
    let mut final_states = collections::BTreeMap::new();
    for line in final_topology {
        let state = snapshot::State::parse(line)?;
        anyhow::ensure!(
            final_states.insert(state.pane, state).is_none(),
            "duplicate pane in final topology"
        );
    }
    anyhow::ensure!(
        final_states.len() == states.len(),
        "topology changed during snapshot"
    );
    for state in &states {
        anyhow::ensure!(
            final_states.get(&state.pane) == Some(state),
            "state changed inside snapshot batch"
        );
    }
    let final_session = &batch.replies[ids.len() * 4 + 1];
    anyhow::ensure!(
        final_session.len() == 1 && final_session[0] == session.to_string(),
        "attached session changed during restore"
    );
    let mut restored = Vec::new();
    for (index, state) in states.into_iter().enumerate() {
        let start = index * 4;
        restored.push(snapshot::Pane::restore(
            state,
            &batch.replies[start + 1],
            &batch.replies[start + 2],
            &batch.replies[start + 3],
            history,
        )?);
    }
    let mut view = snapshot::View::new(session, restored)?;
    for (completed, notification) in batch.notifications {
        if completed < commands.len() {
            // Everything before the enable-output reply is superseded by the
            // capture. Unexpected pane bytes here violate the tested boundary.
            anyhow::ensure!(
                !matches!(
                    notification,
                    tmuxctl::Notification::Output { .. }
                        | tmuxctl::Notification::ExtendedOutput { .. }
                ),
                "pane output appeared before the snapshot boundary"
            );
        } else {
            view.apply(notification);
        }
    }
    Ok(view)
}

// User hooks can yield between otherwise synchronous commands. Reject known
// hazards rather than silently claiming a coherent snapshot. This is a preflight,
// not a lock against another client changing configuration concurrently.
fn verify_snapshot_environment(
    inspector: &mut inspect::Inspector,
    session: tmuxctl::SessionId,
    panes: &[inspect::Pane],
) -> anyhow::Result<()> {
    let aliases = inspector.request("show-options -s -v command-alias\n")?;
    for alias in aliases {
        let name = alias.split_once('=').map(|(name, _)| name).unwrap_or("");
        anyhow::ensure!(
            !matches!(
                name,
                "capture-pane"
                    | "display-message"
                    | "list-panes"
                    | "refresh-client"
                    | "show-hooks"
                    | "show-options"
            ),
            "a command alias overrides snapshot command {name}"
        );
    }
    let mut commands = vec![
        "show-hooks -g".to_owned(),
        "show-hooks -gw".to_owned(),
        format!("show-hooks -t '{session}'"),
    ];
    let windows: collections::BTreeSet<_> = panes.iter().map(|pane| pane.window).collect();
    commands.extend(
        windows
            .iter()
            .map(|window| format!("show-hooks -w -t {window}")),
    );
    commands.extend(
        panes
            .iter()
            .map(|pane| format!("show-hooks -p -t {}", pane.id)),
    );
    let hooks = inspector.request_batch(&commands)?;
    for lines in hooks.replies {
        reject_snapshot_hooks(&lines)?;
    }
    Ok(())
}

fn reject_snapshot_hooks(lines: &[String]) -> anyhow::Result<()> {
    for line in lines {
        let word = line.split_whitespace().next().unwrap_or("");
        let hook = word.split('[').next().unwrap_or("");
        if matches!(
            hook,
            "after-capture-pane"
                | "after-display-message"
                | "after-list-panes"
                | "after-refresh-client"
        ) {
            anyhow::ensure!(
                line == hook,
                "configured {hook} hook prevents a synchronous snapshot"
            );
        }
    }
    Ok(())
}

fn snapshot_commands(panes: &[tmuxctl::PaneId], history: usize) -> Vec<String> {
    let mut commands = Vec::new();
    for pane in panes {
        commands.push(format!(
            "display-message -p -t {pane} '{}'",
            snapshot::STATE_FORMAT
        ));
        // -J preserves the distinction between wrapped lines and hard breaks.
        // -q makes a missing saved primary screen an empty successful capture.
        commands.push(format!(
            "capture-pane -p -e -C -J -N -t {pane} -S -{history} -E -"
        ));
        commands.push(format!(
            "capture-pane -p -a -q -e -C -J -N -t {pane} -S -{history} -E -"
        ));
        commands.push(format!("capture-pane -p -P -C -t {pane}"));
    }
    commands.push(format!("list-panes -s -F '{}'", snapshot::STATE_FORMAT));
    commands.push("display-message -p '#{session_id}'".to_owned());
    // The reply to this LAST command is the snapshot/live cut. tmux queues
    // subsequent pane output behind it, even under a slow SSH reader.
    commands.push("refresh-client -f '!no-output'".to_owned());
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_hooks_are_rejected_without_rejecting_empty_defaults() {
        assert!(reject_snapshot_hooks(&["after-capture-pane".to_owned()]).is_ok());
        assert!(
            reject_snapshot_hooks(&["client-attached[0] display-message hi".to_owned()]).is_ok()
        );
        assert!(
            reject_snapshot_hooks(&["after-capture-pane[0] run-shell 'sleep 1'".to_owned()])
                .is_err()
        );
    }

    #[test]
    fn snapshot_commands_form_one_synchronous_bounded_list() {
        let commands = snapshot_commands(&[tmuxctl::PaneId(1), tmuxctl::PaneId(2)], 200);
        assert_eq!(commands.len(), 11);
        assert!(commands[0].contains("display-message -p -t %1"));
        assert!(commands[4].contains("display-message -p -t %2"));
        assert!(commands[3].contains("-P"));
        assert_eq!(commands.last().unwrap(), "refresh-client -f '!no-output'");
        assert!(commands.iter().all(|line| !line.contains('\n')));
        let maximum: Vec<_> = (0..snapshot::MAX_PANES)
            .map(|id| tmuxctl::PaneId(id.try_into().unwrap()))
            .collect();
        let commands = snapshot_commands(&maximum, snapshot::MAX_HISTORY_LINES);
        assert!(commands.len() <= 256);
        assert!(commands.join(" ; ").len() < 64 * 1024);
    }
}
