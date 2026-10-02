//! Desktop state and a single cancellable SSH worker with bounded user input.
//!
//! The worker never holds the model mutex during SSH or snapshot requests.
//! A replaced request cannot publish into the next connection's view.

use std::{collections, env, path, sync, thread, time};

use crate::{
    core, input, inspect, reconnect, session, sessions, snapshot, ssh, terminal, ui, window,
};

#[derive(Clone)]
pub struct Connection {
    pub options: ssh::Options,
    pub session: core::SessionName,
    pub socket: Option<String>,
    pub history: usize,
    pub access: session::Access,
    /// Retry transport loss automatically. Only transport loss: authentication,
    /// trust, missing-session, and detach never retry regardless of this.
    pub reconnect: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Idle,
    Connecting,
    Watching,
    Resynchronizing,
    /// Waiting out a backoff delay before another attachment attempt. The last
    /// view stays readable; nothing typed here is queued for later delivery.
    Reconnecting,
    Disconnected,
    Failed,
    Demo,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Not connected",
            Self::Connecting => "Connecting",
            Self::Watching => "Connected",
            Self::Resynchronizing => "Resynchronizing",
            Self::Reconnecting => "Reconnecting",
            Self::Disconnected => "Disconnected",
            Self::Failed => "Connection failed",
            Self::Demo => "Demo data",
        }
    }
}

/// Visible retry state. The UI shows the attempt and the remaining wait so an
/// automatic reconnection is never something happening silently behind the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retry {
    pub attempt: u32,
    pub resume_at: time::Instant,
    pub failure: reconnect::Failure,
}

impl Retry {
    pub fn remaining(self) -> time::Duration {
        self.resume_at
            .saturating_duration_since(time::Instant::now())
    }
}

const MAX_PENDING_ACTIONS: usize = 64;
const MAX_PENDING_BYTES: usize = 128 * 1024;

/// A UI action is bound to the exact connection and reconstructed view in which
/// it originated. Pane IDs alone are unsafe across a new tmux server/connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Target {
    epoch: u64,
    generation: u64,
    pane: tmuxctl::PaneId,
}

impl Target {
    pub fn pane(self) -> tmuxctl::PaneId {
        self.pane
    }
}

struct Pending {
    target: Target,
    action: input::Action,
    queued_at: time::Instant,
}

/// What the worker has been asked to do next. Discovery and creation are
/// one-shot queries; they neither disturb nor become an attachment.
pub(crate) enum Request {
    Attach(Connection),
    ListSessions(Connection),
    /// Explicitly start a session, which starts a server if none is running.
    /// Only a button produces this; no failure path ever does.
    CreateSession(Connection, core::Size),
}

/// The outcome of the last discovery request, for the connection form.
#[derive(Clone, Debug)]
pub enum Discovery {
    Running,
    Sessions(Vec<sessions::Summary>),
    Created(String),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MovedSession {
    pub name: String,
    /// Moving the session's only pane destroys the old tmux session and its
    /// control attachment; the workspace should replace that tab, not add one.
    pub source_ended: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InputBacklog {
    /// Actions accepted by the UI but not written to tmux yet.
    pub queued: usize,
    /// Actions written to tmux whose guarded transaction is not acknowledged.
    pub in_flight: usize,
}

impl InputBacklog {
    pub fn total(self) -> usize {
        self.queued + self.in_flight
    }
}

pub(crate) struct State {
    /// Bumped immediately before a worker asks the GUI to reconsider this
    /// client. The workspace uses it to discard duplicate and hidden-tab wakes.
    revision: u64,
    epoch: u64,
    pending: Option<Request>,
    stopping: bool,
    pub generation: u64,
    pub phase: Phase,
    pub view: Option<snapshot::View>,
    pub error: Option<String>,
    pub access: session::Access,
    pub allow_resize: bool,
    /// Set while an automatic reconnection is scheduled. Cleared as soon as an
    /// attempt starts, so a stale countdown is never displayed.
    pub retry: Option<Retry>,
    /// A one-shot report that the reattached session is not the one that was
    /// lost, or that its scrollback is shorter than what was on screen.
    pub continuity: Option<String>,
    /// How the last attachment ended, once it has ended.
    pub failure: Option<reconnect::Failure>,
    /// The last session-discovery result, shown on the connection form.
    pub discovery: Option<Discovery>,
    /// Last interactive-command round trip, from traffic we already send.
    pub last_rtt: Option<time::Duration>,
    /// Breakdown for the last acknowledged ordinary-input transaction.
    pub input_latency: Option<inspect::InputLatency>,
    /// Pending in-band `rename-session` (new name, previous name).
    rename: Option<(core::SessionName, String)>,
    /// The rename is queued or waiting for tmux's bounded reply.
    pub renaming: bool,
    /// Name tmux accepted, for the tab label and saved workspace.
    renamed: Option<String>,
    /// Previous session name, restored if tmux rejected the rename.
    rename_revert: Option<String>,
    /// Completed pane handoff waiting for the workspace to open its new tab.
    moved_session: Option<MovedSession>,
    /// The attached session lost its final window and no longer exists.
    session_closed: bool,
    actions: collections::VecDeque<Pending>,
    action_bytes: usize,
    inflight_input_actions: usize,
    io_wake: Option<ssh::Wake>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            revision: 0,
            epoch: 0,
            pending: None,
            stopping: false,
            generation: 0,
            phase: Phase::Idle,
            view: None,
            error: None,
            access: session::Access::ReadOnly,
            allow_resize: false,
            retry: None,
            continuity: None,
            failure: None,
            discovery: None,
            last_rtt: None,
            input_latency: None,
            rename: None,
            renaming: false,
            renamed: None,
            rename_revert: None,
            moved_session: None,
            session_closed: false,
            actions: collections::VecDeque::new(),
            action_bytes: 0,
            inflight_input_actions: 0,
            io_wake: None,
        }
    }
}

impl State {
    fn cancel(&mut self) {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("connection epoch exhausted");
        self.pending = None;
        self.error = None;
        self.retry = None;
        self.continuity = None;
        self.failure = None;
        self.input_latency = None;
        self.rename = None;
        self.renaming = false;
        self.renamed = None;
        self.rename_revert = None;
        self.moved_session = None;
        self.session_closed = false;
        self.discard_actions();
        self.access = session::Access::ReadOnly;
        self.allow_resize = false;
        if let Some(wake) = self.io_wake.take() {
            wake.notify();
        }
        if let Some(ref mut view) = self.view {
            view.disconnect();
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// An interactive demo view for tests that need input tokens without a
    /// network connection. Never reachable from a running application.
    #[cfg(test)]
    pub(crate) fn interactive_demo() -> anyhow::Result<Self> {
        Ok(Self {
            phase: Phase::Watching,
            view: Some(demo_view()?),
            access: session::Access::Interactive,
            ..Self::default()
        })
    }

    pub(crate) fn input_ready(&self) -> bool {
        self.phase == Phase::Watching
            && self.access == session::Access::Interactive
            && self
                .view
                .as_ref()
                .is_some_and(|view| view.status() == snapshot::Status::Watching)
    }

    pub(crate) fn target(&self, pane: tmuxctl::PaneId) -> Option<Target> {
        (self.input_ready() && self.view.as_ref()?.panes().contains_key(&pane)).then_some(Target {
            epoch: self.epoch,
            generation: self.generation,
            pane,
        })
    }

    fn discard_actions(&mut self) {
        self.actions.clear();
        self.action_bytes = 0;
        self.inflight_input_actions = 0;
    }

    pub(crate) fn input_backlog(&self) -> InputBacklog {
        InputBacklog {
            queued: self
                .actions
                .iter()
                .filter(|pending| {
                    matches!(
                        pending.action,
                        input::Action::Bytes(_) | input::Action::Key(..)
                    )
                })
                .count(),
            in_flight: self.inflight_input_actions,
        }
    }

    pub(crate) fn enqueue(&mut self, target: Target, action: input::Action) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.target(target.pane) == Some(target),
            "input was not sent: the connection or layout is no longer current"
        );
        anyhow::ensure!(
            !action.changes_window_size() || self.allow_resize,
            "remote resizing is disabled; it changes the shared tmux layout"
        );
        session::validate_action(
            self.view.as_ref().expect("target validated"),
            target.pane,
            &action,
        )?;
        let size = action.size();
        anyhow::ensure!(
            self.actions.len() < MAX_PENDING_ACTIONS
                && self.action_bytes + size <= MAX_PENDING_BYTES,
            "input queue is full; this action was not sent"
        );
        // Coalesce ordinary text, without moving it past a key/paste/resize.
        if let input::Action::Bytes(ref bytes) = action
            && let Some(Pending {
                target: previous,
                action: input::Action::Bytes(queued),
                ..
            }) = self.actions.back_mut()
            && *previous == target
            && queued.len() + bytes.len() <= crate::command::MAX_INPUT_BYTES
        {
            queued.extend_from_slice(bytes);
        } else {
            self.actions.push_back(Pending {
                target,
                action,
                queued_at: time::Instant::now(),
            });
        }
        self.action_bytes += size;
        if let Some(ref wake) = self.io_wake {
            wake.notify();
        }
        Ok(())
    }

    fn accepts(&self, epoch: u64) -> bool {
        !self.stopping && self.epoch == epoch
    }

    /// Take a fresh epoch for the next attachment attempt, unless the user has
    /// already superseded this connection. Every outstanding input token becomes
    /// invalid, so nothing produced against the lost attachment can be delivered.
    ///
    /// Unlike `cancel`, this keeps the user's per-connection resize consent: it
    /// is the same profile and session, and every resize is still guarded
    /// server-side against the exact geometry it was aimed at.
    fn renew(&mut self, previous: u64) -> Option<u64> {
        if !self.accepts(previous) {
            return None;
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("connection epoch exhausted");
        self.discard_actions();
        self.retry = None;
        Some(self.epoch)
    }
}

type Shared = sync::Arc<(sync::Mutex<State>, sync::Condvar)>;
type Wake = sync::Arc<dyn Fn() + Send + Sync>;

pub struct Client {
    shared: Shared,
    worker: Option<thread::JoinHandle<()>>,
    wake: Wake,
}

impl Client {
    pub fn new(wake: Wake) -> std::io::Result<Self> {
        let shared = sync::Arc::new((sync::Mutex::new(State::default()), sync::Condvar::new()));
        let notify_shared = sync::Arc::clone(&shared);
        let notify: Wake = sync::Arc::new(move || {
            let mut state = notify_shared
                .0
                .lock()
                .unwrap_or_else(sync::PoisonError::into_inner);
            state.revision = state.revision.wrapping_add(1);
            drop(state);
            wake();
        });
        let worker_shared = sync::Arc::clone(&shared);
        let worker_wake = sync::Arc::clone(&notify);
        let worker = thread::Builder::new()
            .name("starcom-ssh".to_owned())
            .spawn(move || worker_loop(worker_shared, worker_wake))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            wake: notify,
        })
    }

    pub fn connect(&self, connection: Connection) -> anyhow::Result<()> {
        connection.options.validate()?;
        anyhow::ensure!(
            connection.history <= snapshot::MAX_HISTORY_LINES,
            "history exceeds budget"
        );
        let mut state = self.lock();
        state.cancel();
        state.phase = Phase::Connecting;
        state.pending = Some(Request::Attach(connection));
        drop(state);
        self.shared.1.notify_one();
        (self.wake)();
        Ok(())
    }

    /// Ask the host which sessions exist. This runs on the worker, opens its own
    /// short-lived connection, and cannot start a tmux server.
    pub fn list_sessions(&self, connection: Connection) -> anyhow::Result<()> {
        self.query(Request::ListSessions(connection))
    }

    /// Explicitly create a session. Unlike every other path in Starcom, this may
    /// start a tmux server — which is why only a deliberate action reaches it.
    pub fn create_session(&self, connection: Connection, size: core::Size) -> anyhow::Result<()> {
        self.query(Request::CreateSession(connection, size))
    }

    fn query(&self, request: Request) -> anyhow::Result<()> {
        let connection = match request {
            Request::Attach(ref connection)
            | Request::ListSessions(ref connection)
            | Request::CreateSession(ref connection, _) => connection,
        };
        connection.options.validate()?;
        let mut state = self.lock();
        anyhow::ensure!(
            !matches!(
                state.phase,
                Phase::Connecting | Phase::Watching | Phase::Resynchronizing | Phase::Reconnecting
            ),
            "disconnect before asking the host about its sessions"
        );
        anyhow::ensure!(
            state.pending.is_none(),
            "a request to this host is already running"
        );
        state.discovery = Some(Discovery::Running);
        state.pending = Some(request);
        drop(state);
        self.shared.1.notify_one();
        (self.wake)();
        Ok(())
    }

    pub fn discovery(&self) -> Option<Discovery> {
        self.lock().discovery.clone()
    }

    pub fn clear_discovery(&self) {
        self.lock().discovery = None;
    }

    pub fn disconnect(&self) {
        let mut state = self.lock();
        state.cancel();
        state.phase = Phase::Disconnected;
        drop(state);
        self.shared.1.notify_one();
        (self.wake)();
    }

    pub fn demo(&self) -> anyhow::Result<()> {
        let view = demo_view()?;
        let mut state = self.lock();
        state.cancel();
        state.view = Some(view);
        state.generation += 1;
        state.phase = Phase::Demo;
        drop(state);
        self.shared.1.notify_one();
        (self.wake)();
        Ok(())
    }

    /// Obtain a token only after restoration. Keep it with any delayed GUI
    /// action; never recreate a token on retry.
    pub fn target(&self, pane: tmuxctl::PaneId) -> Option<Target> {
        self.lock().target(pane)
    }

    pub fn submit(&self, target: Target, action: input::Action) -> anyhow::Result<()> {
        self.lock().enqueue(target, action)
    }

    /// Queue one rename on the existing control attachment. The command is
    /// never retried: an uncertain completion instead reconnects to the
    /// optimistically saved new name.
    pub(crate) fn rename_session(
        &self,
        name: core::SessionName,
        previous: String,
    ) -> anyhow::Result<()> {
        let mut state = self.lock();
        anyhow::ensure!(
            state.input_ready(),
            "connect with an interactive session to rename it"
        );
        anyhow::ensure!(!state.renaming, "a session rename is already running");
        state.rename = Some((name, previous));
        state.renaming = true;
        state.renamed = None;
        state.rename_revert = None;
        if let Some(ref wake) = state.io_wake {
            wake.notify();
        }
        drop(state);
        self.shared.1.notify_one();
        Ok(())
    }

    pub(crate) fn take_renamed(&self) -> Option<String> {
        self.lock().renamed.take()
    }

    pub(crate) fn take_rename_revert(&self) -> Option<String> {
        self.lock().rename_revert.take()
    }

    pub(crate) fn take_moved_session(&self) -> Option<MovedSession> {
        self.lock().moved_session.take()
    }

    pub(crate) fn take_session_closed(&self) -> bool {
        std::mem::take(&mut self.lock().session_closed)
    }

    #[cfg(test)]
    pub(crate) fn mark_session_closed_for_test(&self) {
        self.lock().session_closed = true;
    }

    /// Admit a GUI frame atomically, so a full queue cannot accept half of a
    /// committed UTF-8 string or reorder a paste and its following keys.
    pub(crate) fn submit_batch(&self, actions: Vec<(Target, input::Action)>) -> anyhow::Result<()> {
        let mut state = self.lock();
        let size: usize = actions.iter().map(|(_, action)| action.size()).sum();
        anyhow::ensure!(
            state.actions.len() + actions.len() <= MAX_PENDING_ACTIONS
                && state.action_bytes + size <= MAX_PENDING_BYTES,
            "input queue is full; this frame's actions were not sent"
        );
        for (target, action) in &actions {
            anyhow::ensure!(
                state.target(target.pane) == Some(*target),
                "input target changed; nothing was sent"
            );
            anyhow::ensure!(
                !action.changes_window_size() || state.allow_resize,
                "remote resizing is disabled"
            );
            session::validate_action(
                state.view.as_ref().expect("target checked"),
                target.pane,
                action,
            )?;
        }
        for (target, action) in actions {
            state.enqueue(target, action)?;
        }
        Ok(())
    }

    /// Explicit per-connection consent; automatic window sizing remains off.
    pub fn allow_remote_resize(&self, allow: bool) {
        let mut state = self.lock();
        state.allow_resize = allow;
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase
    }

    /// Unblock a socket wait so the worker can notice a machine suspend.
    /// Does not enqueue input and does not change the connection epoch.
    pub(crate) fn nudge(&self) {
        if let Some(ref wake) = self.lock().io_wake {
            wake.notify();
        }
    }

    /// Bumped for every reconstructed view. A change means the models were
    /// rebuilt from a fresh snapshot, never appended to the previous ones.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// The scheduled reconnection attempt, when one is pending.
    pub fn retry(&self) -> Option<Retry> {
        self.lock().retry
    }

    /// How the last attachment ended. `None` while an attachment is healthy.
    pub fn failure(&self) -> Option<reconnect::Failure> {
        self.lock().failure
    }

    /// The last failure's text, as shown to the user. Bounded, and already
    /// escaped where it came from the remote host.
    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }

    /// A one-shot report that the reattached session or its scrollback is not
    /// continuous with what was on screen before.
    pub fn continuity(&self) -> Option<String> {
        self.lock().continuity.clone()
    }

    /// Inspect models without exposing the lock or allowing a borrowed view to
    /// outlive it. No networking runs under this lock.
    pub fn with_view<R>(&self, read: impl FnOnce(Option<&snapshot::View>) -> R) -> R {
        read(self.lock().view.as_ref())
    }

    pub(crate) fn lock(&self) -> sync::MutexGuard<'_, State> {
        self.shared
            .0
            .lock()
            .unwrap_or_else(sync::PoisonError::into_inner)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut state = self.lock();
        state.cancel();
        state.stopping = true;
        drop(state);
        self.shared.1.notify_one();
        // Normal channel polling stops within its bounded wait. DNS and local
        // file/agent setup may block outside network operation deadlines; do not freeze window
        // closure waiting for those. There is only one worker, not one per retry.
        if let Some(worker) = self.worker.take()
            && worker.is_finished()
        {
            let _ = worker.join();
        }
    }
}

/// Why one attachment stopped running.
enum Outcome {
    /// The user replaced or cancelled this connection. Say nothing further.
    Cancelled,
    /// tmux ended the control session and said why.
    Ended(reconnect::Failure),
    /// The attached session's final window closed. Its tab has no recoverable
    /// remote object and should disappear rather than become a red tombstone.
    SessionClosed,
    /// A non-idempotent rename was delivered but its final reply was not
    /// observed. Reconnect once to the already-saved new name; never resend it.
    RenameReconnect,
}

fn worker_loop(shared: Shared, wake: Wake) {
    loop {
        let (mut epoch, request) = {
            let state = shared
                .0
                .lock()
                .unwrap_or_else(sync::PoisonError::into_inner);
            let mut state = shared
                .1
                .wait_while(state, |state| !state.stopping && state.pending.is_none())
                .unwrap_or_else(sync::PoisonError::into_inner);
            if state.stopping {
                return;
            }
            (
                state.epoch,
                state.pending.take().expect("request checked above"),
            )
        };
        let mut connection = match request {
            Request::Attach(connection) => connection,
            // One-shot queries: run, publish the answer, wait for the next
            // request. Neither one becomes or disturbs an attachment.
            other => {
                let outcome = match other {
                    Request::ListSessions(ref connection) => {
                        sessions::list(&connection.options, connection.socket.as_deref())
                            .map(Discovery::Sessions)
                    }
                    Request::CreateSession(ref connection, size) => sessions::create(
                        &connection.options,
                        connection.socket.as_deref(),
                        &connection.session,
                        size,
                    )
                    .map(|()| Discovery::Created(connection.session.as_str().to_owned())),
                    Request::Attach(_) => unreachable!("attach handled above"),
                };
                let mut state = shared
                    .0
                    .lock()
                    .unwrap_or_else(sync::PoisonError::into_inner);
                if state.accepts(epoch) {
                    state.discovery = Some(match outcome {
                        Ok(discovery) => discovery,
                        // Bounded plain text for a GUI label; the remote half is
                        // already escaped where it was read.
                        Err(error) => {
                            Discovery::Failed(format!("{error:#}").chars().take(1024).collect())
                        }
                    });
                }
                drop(state);
                wake();
                continue;
            }
        };
        // One backoff schedule per user-requested connection, so a session that
        // flaps repeatedly keeps backing off instead of hammering every 500 ms.
        let mut backoff = reconnect::Backoff::new(jitter_seed(epoch));
        let mut previous_identity = None;
        loop {
            let result = watch(
                &shared,
                &wake,
                epoch,
                &mut connection,
                &mut previous_identity,
                &mut backoff,
            );
            let (failure, detail) = match result {
                Ok(Outcome::Cancelled) => break,
                Ok(Outcome::SessionClosed) => {
                    report_session_closed(&shared, &wake, epoch);
                    break;
                }
                Ok(Outcome::RenameReconnect) => match renew_epoch(&shared, &wake, epoch) {
                    Some(renewed) => {
                        epoch = renewed;
                        continue;
                    }
                    None => break,
                },
                Ok(Outcome::Ended(failure)) => (failure, failure.summary().to_owned()),
                Err(error) => {
                    let failure = reconnect::classify(&error);
                    let session_ended = {
                        let state = shared
                            .0
                            .lock()
                            .unwrap_or_else(sync::PoisonError::into_inner);
                        state.accepts(epoch) && ended_last_pane(state.view.as_ref(), failure)
                    };
                    if session_ended {
                        report_session_closed(&shared, &wake, epoch);
                        break;
                    }
                    (
                        failure,
                        // Do not emit credentials or remote output to logs. Error
                        // display is plain GUI text, bounded independently of the wire.
                        format!("{error:#}").chars().take(2048).collect(),
                    )
                }
            };
            let retriable = failure.retriable() && connection.reconnect;
            let delay = retriable.then(|| backoff.next_delay());
            let scheduled = report_failure(
                &shared,
                &wake,
                epoch,
                failure,
                &detail,
                delay.map(|delay| (backoff.attempt(), delay)),
            );
            // "That session does not exist" is the one failure where the list of
            // sessions is the missing information. The user already asked to
            // connect and already authenticated, so ask once and show it rather
            // than making them press a button to learn what went wrong. Every
            // other failure either cannot list (auth, trust) or already says
            // what happened, so nothing else triggers this.
            if scheduled.is_none() && failure == reconnect::Failure::MissingSession {
                list_after_missing_session(&shared, &wake, epoch, &connection);
            }
            let Some(next) = scheduled else {
                break;
            };
            if !wait_for_retry(&shared, epoch, next) {
                break;
            }
            match renew_epoch(&shared, &wake, epoch) {
                Some(renewed) => epoch = renewed,
                None => break,
            }
        }
    }
}

fn ended_last_pane(view: Option<&snapshot::View>, failure: reconnect::Failure) -> bool {
    view.is_some_and(|view| {
        view.window_closed()
            // On the affected tmux path the final pane produces neither
            // `%window-close` nor `%exit`; the local tmux front-end instead
            // ends mid-control framing. Restrict that fallback to a coherent
            // one-pane snapshot and a protocol teardown, so an ordinary SSH
            // transport loss still reconnects and an explicit detach remains.
            || (failure == reconnect::Failure::Protocol && view.panes().len() == 1)
    })
}

fn report_session_closed(shared: &Shared, wake: &Wake, epoch: u64) {
    let mut state = shared
        .0
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner);
    if state.accepts(epoch) {
        state.phase = Phase::Disconnected;
        state.failure = Some(reconnect::Failure::MissingSession);
        state.error = None;
        state.io_wake = None;
        state.discard_actions();
        state.session_closed = true;
    }
    drop(state);
    wake();
}

/// Decorrelate one tab's retry schedule from another's. This is a scheduling
/// nicety, not a secret: nothing about the connection is derivable from it.
fn jitter_seed(epoch: u64) -> u64 {
    let since = time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos() as u64);
    since.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ epoch.wrapping_add(1)
}

/// Publish why the attachment ended. Returns the instant of the next attempt
/// when one is scheduled, or None when this connection is finished.
fn report_failure(
    shared: &Shared,
    wake: &Wake,
    epoch: u64,
    failure: reconnect::Failure,
    detail: &str,
    retry: Option<(u32, time::Duration)>,
) -> Option<time::Instant> {
    let mut state = shared
        .0
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner);
    if !state.accepts(epoch) {
        return None;
    }
    // The last view stays readable and copyable; only its liveness is revoked.
    if let Some(ref mut view) = state.view {
        view.disconnect();
    }
    // Nothing typed against the lost attachment is kept for later delivery.
    state.discard_actions();
    state.io_wake = None;
    let scheduled = retry.map(|(attempt, delay)| {
        let resume_at = time::Instant::now() + delay;
        state.phase = Phase::Reconnecting;
        state.retry = Some(Retry {
            attempt,
            resume_at,
            failure,
        });
        resume_at
    });
    if scheduled.is_none() {
        state.retry = None;
        if let Some((_, previous)) = state.rename.take() {
            state.rename_revert = Some(previous);
        }
        state.renaming = false;
        state.phase = match failure {
            // An orderly detach is not an error to apologize for.
            reconnect::Failure::Detached => Phase::Disconnected,
            _ => Phase::Failed,
        };
    }
    state.failure = Some(failure);
    state.error = Some(
        format!("{} {detail}", failure.summary())
            .chars()
            .take(2048)
            .collect(),
    );
    drop(state);
    wake();
    scheduled
}

/// Ask the host what sessions it does have, after an attach found none.
///
/// This is a read-only `tmux -N` query on its own short-lived connection: it
/// cannot start a server, and it never becomes an attachment. It runs only
/// after a user-initiated connect failed for this one reason.
fn list_after_missing_session(shared: &Shared, wake: &Wake, epoch: u64, connection: &Connection) {
    {
        let mut state = shared
            .0
            .lock()
            .unwrap_or_else(sync::PoisonError::into_inner);
        if !state.accepts(epoch) {
            return;
        }
        state.discovery = Some(Discovery::Running);
    }
    wake();
    let found = sessions::list(&connection.options, connection.socket.as_deref());
    let mut state = shared
        .0
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner);
    if !state.accepts(epoch) {
        return;
    }
    state.discovery = Some(match found {
        Ok(found) => Discovery::Sessions(found),
        // The attach failure is the headline; this is a failed follow-up, so
        // do not overwrite the error the user is already reading.
        Err(error) => Discovery::Failed(format!("{error:#}").chars().take(1024).collect()),
    });
    drop(state);
    wake();
}

/// Sleep until `until`, waking immediately if the user cancels or reconnects.
/// Returns false when this connection was superseded while waiting.
fn wait_for_retry(shared: &Shared, epoch: u64, until: time::Instant) -> bool {
    let mut state = shared
        .0
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner);
    loop {
        if !state.accepts(epoch) {
            return false;
        }
        let Some(remaining) = until.checked_duration_since(time::Instant::now()) else {
            return true;
        };
        if remaining.is_zero() {
            return true;
        }
        state = shared
            .1
            .wait_timeout(state, remaining)
            .unwrap_or_else(sync::PoisonError::into_inner)
            .0;
    }
}

/// Move to a fresh epoch and announce the attempt. Returns None if the user
/// superseded this connection between the wait and the retry.
fn renew_epoch(shared: &Shared, wake: &Wake, epoch: u64) -> Option<u64> {
    let mut state = shared
        .0
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner);
    let renewed = state.renew(epoch)?;
    state.phase = Phase::Connecting;
    drop(state);
    wake();
    Some(renewed)
}

fn take_ordinary_input(
    state: &mut State,
) -> anyhow::Result<Option<(input::Target, Vec<input::Action>, time::Duration)>> {
    // A rename is a session-scoped ordering barrier. Once requested, drain
    // input already on the wire instead of letting newly typed input postpone
    // it indefinitely.
    if !state.input_ready() || state.rename.is_some() {
        return Ok(None);
    }
    loop {
        let Some(front) = state.actions.front() else {
            return Ok(None);
        };
        if !matches!(
            front.action,
            input::Action::Bytes(_) | input::Action::Key(..)
        ) {
            return Ok(None);
        }
        let pending = state.actions.pop_front().expect("front checked");
        state.action_bytes -= pending.action.size();
        if state.target(pending.target.pane) != Some(pending.target) {
            continue;
        }
        let target = session::action_target(
            state.view.as_ref().expect("input-ready view"),
            pending.target.pane,
        )?;
        let queue = time::Instant::now().saturating_duration_since(pending.queued_at);
        let mut actions = vec![pending.action];
        while actions.len() < 32
            && state.actions.front().is_some_and(|next| {
                next.target == pending.target
                    && matches!(
                        next.action,
                        input::Action::Bytes(_) | input::Action::Key(..)
                    )
            })
        {
            let next = state.actions.pop_front().expect("front checked");
            state.action_bytes -= next.action.size();
            actions.push(next.action);
        }
        return Ok(Some((target, actions, queue)));
    }
}

fn slow_input(latency: inspect::InputLatency) -> bool {
    latency.queue >= time::Duration::from_millis(100)
        || latency.control >= time::Duration::from_millis(100)
        || latency.max_output_lag_ms >= 100
}

fn watch(
    shared: &Shared,
    wake: &Wake,
    epoch: u64,
    connection: &mut Connection,
    previous: &mut Option<inspect::Identity>,
    backoff: &mut reconnect::Backoff,
) -> anyhow::Result<Outcome> {
    let connection_timer = navigato_support::timer(navigato_support::Metric::Connection);
    let attached = session::Session::attach_with_access(
        &connection.options,
        &connection.session,
        connection.socket.as_deref(),
        connection.history,
        connection.access,
    )?;
    let (mut inspector, mut view) = attached.into_parts();
    let session_id = view.session;
    // Attaching is by name, so a restarted server hands back a session that
    // merely shares that name. A session id alone cannot see that: a fresh tmux
    // server numbers its first session $0 again. Compare the whole identity.
    let identity = inspector.identity()?;
    anyhow::ensure!(
        identity.session == session_id,
        "the attached session changed while identifying it"
    );
    let continuity = match *previous {
        Some(lost) if lost != identity => Some(if lost.server == identity.server {
            format!(
                "Reattached to a different tmux session ({} replaced {}). The earlier session and its scrollback are gone.",
                identity.session, lost.session
            )
        } else {
            "The remote tmux server restarted. This is a new session that only shares the old name; the earlier session and its scrollback are gone."
                .to_owned()
        }),
        _ => None,
    };
    drop(connection_timer);
    navigato_support::feature(navigato_support::Feature::Remote);
    if !connection.options.jumps.is_empty() {
        navigato_support::feature(navigato_support::Feature::Jump);
    }
    *previous = Some(identity);
    {
        let mut state = shared
            .0
            .lock()
            .unwrap_or_else(sync::PoisonError::into_inner);
        if !state.accepts(epoch) {
            return Ok(Outcome::Cancelled);
        }
        if let Some(ref previous) = state.view {
            view.preserve_history_offsets(previous);
        }
        state.view = Some(view);
        state.access = connection.access;
        state.io_wake = Some(inspector.waker());
        state.generation += 1;
        state.last_rtt = inspector.last_rtt;
        state.phase = Phase::Watching;
        state.retry = None;
        state.error = None;
        state.failure = None;
        state.continuity = continuity;
        if connection.access == session::Access::Interactive {
            state.allow_resize = true;
        }
    }
    // A fully restored attachment earns a fresh schedule: the next drop starts
    // from the short delay again instead of inheriting an old backoff.
    backoff.reset();
    let mut last_alive = reconnect::AliveClock::now();
    wake();
    loop {
        // Rename before resync/input. It is session-scoped and does not depend
        // on the current pane geometry.
        let rename = {
            let mut state = shared
                .0
                .lock()
                .unwrap_or_else(sync::PoisonError::into_inner);
            if !state.accepts(epoch) {
                return Ok(Outcome::Cancelled);
            }
            inspector
                .input_idle()
                .then(|| state.rename.take())
                .flatten()
        };
        if let Some((name, previous)) = rename {
            // Once written, the command must never be replayed. Point any
            // recovery at the optimistic name before attempting delivery.
            connection.session = name.clone();
            match inspector.rename_session(session_id, &name) {
                Ok(notifications) => {
                    let mut state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if state.accepts(epoch) {
                        state.renaming = false;
                        state.renamed = Some(name.as_str().to_owned());
                        state.rename_revert = None;
                        state.last_rtt = inspector.last_rtt;
                        state.error = None;
                        if let Some(ref mut view) = state.view {
                            for event in notifications {
                                view.apply(event);
                            }
                        }
                    }
                    // `wake` increments the revision by taking this same
                    // mutex. Calling it with the guard alive self-deadlocks
                    // the worker and then blocks the UI behind it.
                    drop(state);
                    wake();
                }
                Err(error) if reconnect::classify(&error) == reconnect::Failure::Transport => {
                    let mut state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if state.accepts(epoch) {
                        state.renaming = false;
                        state.error = Some(
                            "The rename was delivered but its completion was not observed; reconnecting to the new name."
                                .to_owned(),
                        );
                    }
                    drop(state);
                    wake();
                    return Ok(Outcome::RenameReconnect);
                }
                Err(error) => {
                    connection.session = core::SessionName::new(previous.clone())?;
                    let mut state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if state.accepts(epoch) {
                        state.renaming = false;
                        state.error = Some(format!("Could not rename session: {error}"));
                        state.rename_revert = Some(previous);
                    }
                    drop(state);
                    wake();
                }
            }
            continue;
        }

        // Ordinary input is the latency-sensitive path. Keep several guarded
        // transactions in flight, but never move one across a paste, resize,
        // layout action, or rename. UI wakes interrupt `poll_input`, allowing a
        // newly typed key to be written without waiting for an earlier reply.
        while inspector.can_pipeline_input() {
            let next = {
                let mut state = shared
                    .0
                    .lock()
                    .unwrap_or_else(sync::PoisonError::into_inner);
                if !state.accepts(epoch) {
                    return Ok(Outcome::Cancelled);
                }
                take_ordinary_input(&mut state)?
            };
            let Some((target, actions, queue)) = next else {
                break;
            };
            let action_count = actions.len();
            inspector.start_input(target, &actions, queue)?;
            let mut state = shared
                .0
                .lock()
                .unwrap_or_else(sync::PoisonError::into_inner);
            if !state.accepts(epoch) {
                return Ok(Outcome::Cancelled);
            }
            state.inflight_input_actions =
                state
                    .inflight_input_actions
                    .checked_add(action_count)
                    .ok_or_else(|| anyhow::anyhow!("input backlog overflow"))?;
            drop(state);
            wake();
        }
        if !inspector.input_idle() {
            let progress = inspector.poll_input()?;
            let had_network = progress.received;
            let mut state = shared
                .0
                .lock()
                .unwrap_or_else(sync::PoisonError::into_inner);
            if !state.accepts(epoch) {
                return Ok(Outcome::Cancelled);
            }
            let mut changed = !progress.notifications.is_empty();
            if let Some(ref mut view) = state.view {
                for event in progress.notifications {
                    view.apply(event);
                }
            }
            for completion in progress.completions {
                changed = true;
                state.inflight_input_actions = state
                    .inflight_input_actions
                    .checked_sub(completion.actions)
                    .ok_or_else(|| anyhow::anyhow!("input acknowledgment exceeded backlog"))?;
                state.last_rtt = Some(completion.latency.control);
                state.input_latency = Some(completion.latency);
                if slow_input(completion.latency) {
                    log::warn!(
                        "slow terminal input: queue={}ms control={}ms output={}B/{} events tmux_lag={}ms",
                        completion.latency.queue.as_millis(),
                        completion.latency.control.as_millis(),
                        completion.latency.output_bytes,
                        completion.latency.output_events,
                        completion.latency.max_output_lag_ms,
                    );
                }
                if !completion.applied {
                    state.error = Some(
                        "tmux blocked input because the pane or layout changed. Nothing was retried."
                            .to_owned(),
                    );
                    if let Some(ref mut view) = state.view {
                        view.invalidate();
                    }
                }
            }
            drop(state);
            // `poll_input` also returns when the UI wakes the SSH channel. A
            // local wake is not evidence that the remote stream survived a
            // suspend.
            if had_network {
                last_alive = reconnect::AliveClock::now();
            }
            if changed {
                wake();
            }
            continue;
        }
        let status = {
            let state = shared
                .0
                .lock()
                .unwrap_or_else(sync::PoisonError::into_inner);
            if !state.accepts(epoch) {
                return Ok(Outcome::Cancelled);
            }
            state.view.as_ref().expect("view published").status()
        };
        match status {
            snapshot::Status::Disconnected => {
                // tmux ended the control session. Its reason decides whether
                // reattaching would restore this session or silently land on a
                // different one; the caller applies the retry policy.
                let state = shared
                    .0
                    .lock()
                    .unwrap_or_else(sync::PoisonError::into_inner);
                if !state.accepts(epoch) {
                    return Ok(Outcome::Cancelled);
                }
                let view = state.view.as_ref().expect("view published");
                if view.session_closed() {
                    return Ok(Outcome::SessionClosed);
                }
                let failure = reconnect::classify_exit(
                    view.exit_reason().and_then(snapshot::ExitReason::as_deref),
                );
                drop(state);
                return Ok(Outcome::Ended(failure));
            }
            snapshot::Status::NeedsResync => {
                {
                    let mut state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if !state.accepts(epoch) {
                        return Ok(Outcome::Cancelled);
                    }
                    if !state.actions.is_empty() {
                        state.error = Some(
                            "Layout changed; queued input was discarded, not replayed.".to_owned(),
                        );
                    }
                    state.discard_actions();
                    state.phase = Phase::Resynchronizing;
                }
                wake();
                let mut restored =
                    session::restore(&mut inspector, session_id, connection.history)?;
                if connection.access == session::Access::Interactive {
                    for event in inspector.enable_input(&restored)? {
                        restored.apply(event);
                    }
                }
                let mut state = shared
                    .0
                    .lock()
                    .unwrap_or_else(sync::PoisonError::into_inner);
                if !state.accepts(epoch) {
                    return Ok(Outcome::Cancelled);
                }
                if let Some(ref previous) = state.view {
                    restored.preserve_history_offsets(previous);
                }
                state.view = Some(restored);
                state.generation += 1;
                state.last_rtt = inspector.last_rtt;
                state.phase = Phase::Watching;
                state.continuity = None;
                // A successful replacement snapshot resolves transient layout
                // and guarded-action errors from the stale view.
                state.error = None;
                drop(state);
                last_alive = reconnect::AliveClock::now();
                wake();
            }
            snapshot::Status::Watching => {
                if last_alive.suspended() {
                    return Err(ssh::Error::timeout(
                        "the machine slept; the control stream is not known to have survived",
                    )
                    .into());
                }
                let pending = {
                    let mut state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if !state.accepts(epoch) {
                        return Ok(Outcome::Cancelled);
                    }
                    if let Some(pending) = state.actions.pop_front() {
                        state.action_bytes -= pending.action.size();
                        if let input::Action::ClientSize(size) = pending.action {
                            if !state.allow_resize {
                                continue;
                            }
                            drop(state);
                            let mut notifications = inspector.set_client_size(size)?;
                            // SIGWINCH redraws must complete before restore's
                            // no-output+capture, or a TUI is frozen mid-frame
                            // (wrapped tables, missing lines).
                            notifications.extend(
                                inspector.drain_notifications(inspector.resize_drain_until())?,
                            );
                            last_alive = reconnect::AliveClock::now();
                            let mut state = shared
                                .0
                                .lock()
                                .unwrap_or_else(sync::PoisonError::into_inner);
                            if !state.accepts(epoch) {
                                return Ok(Outcome::Cancelled);
                            }
                            if let Some(ref mut view) = state.view {
                                for event in notifications {
                                    view.apply(event);
                                }
                                view.invalidate();
                            }
                            state.phase = Phase::Resynchronizing;
                            state.discard_actions();
                            state.last_rtt = inspector.last_rtt;
                            continue;
                        }
                        if state.target(pending.target.pane) != Some(pending.target)
                            || (pending.action.changes_window_size() && !state.allow_resize)
                        {
                            continue;
                        }
                        let target = session::action_target(
                            state.view.as_ref().expect("view published"),
                            pending.target.pane,
                        )?;
                        let resizing = pending.action.changes_layout();
                        let source_ended =
                            matches!(pending.action, input::Action::MoveToNewSession)
                                && state.view.as_ref().expect("view published").panes().len() == 1;
                        // Ordinary input is consumed by the bounded pipeline
                        // above. Reaching this synchronous path with it would
                        // reintroduce one-acknowledgment-per-input latency.
                        anyhow::ensure!(
                            !matches!(
                                pending.action,
                                input::Action::Bytes(_) | input::Action::Key(..)
                            ),
                            "ordinary input bypassed the pipeline"
                        );
                        let actions = vec![pending.action];
                        Some((target, actions, resizing, source_ended))
                    } else {
                        None
                    }
                };
                if let Some((target, actions, resizing, source_ended)) = pending {
                    // The pop above is the dispatch boundary. Cancellation may
                    // follow while I/O is in flight; these actions are NEVER requeued.
                    if matches!(actions.as_slice(), [input::Action::MoveToNewSession]) {
                        let moved = inspector.move_to_new_session(target, target.size)?;
                        let mut state = shared
                            .0
                            .lock()
                            .unwrap_or_else(sync::PoisonError::into_inner);
                        if !state.accepts(epoch) {
                            return Ok(Outcome::Cancelled);
                        }
                        for event in moved.notifications {
                            state.view.as_mut().expect("view published").apply(event);
                        }
                        if !moved.applied {
                            state.error = Some(
                                "tmux blocked the pane move because its layout changed. Nothing was retried."
                                    .to_owned(),
                            );
                            state.view.as_mut().expect("view published").invalidate();
                            state.phase = Phase::Resynchronizing;
                        } else {
                            state.moved_session = Some(MovedSession {
                                name: moved.session.as_str().to_owned(),
                                source_ended,
                            });
                            state.discard_actions();
                            if source_ended {
                                state.view.as_mut().expect("view published").disconnect();
                                state.phase = Phase::Disconnected;
                                state.io_wake = None;
                            } else {
                                state.view.as_mut().expect("view published").invalidate();
                                state.phase = Phase::Resynchronizing;
                            }
                        }
                        state.last_rtt = inspector.last_rtt;
                        drop(state);
                        wake();
                        if source_ended && moved.applied {
                            return Ok(Outcome::Cancelled);
                        }
                        continue;
                    }
                    let mut outcome = inspector.interact(target, &actions)?;
                    if resizing && outcome.applied {
                        outcome
                            .notifications
                            .extend(inspector.drain_notifications(inspector.resize_drain_until())?);
                    }
                    let mut state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if !state.accepts(epoch) {
                        return Ok(Outcome::Cancelled);
                    }
                    for event in outcome.notifications {
                        state.view.as_mut().expect("view published").apply(event);
                    }
                    if resizing && outcome.applied {
                        state.view.as_mut().expect("view published").invalidate();
                        state.phase = Phase::Resynchronizing;
                        if !state.actions.is_empty() {
                            state.error = Some(
                                "Layout changed; queued input was discarded, not replayed."
                                    .to_owned(),
                            );
                        }
                        state.discard_actions();
                    }
                    if !outcome.applied {
                        state.error = Some("tmux blocked this action: the pane changed, is in a mode, or synchronize-panes/zoom is enabled. Nothing was retried.".to_owned());
                        state.view.as_mut().expect("view published").invalidate();
                    }
                    state.last_rtt = inspector.last_rtt;
                    drop(state);
                    last_alive = reconnect::AliveClock::now();
                    wake();
                    continue;
                }
                // Socket readiness wait, not a repaint timer. Idle reads do not
                // wake the UI. All network I/O is outside the model mutex.
                // A pending DECSET 2026 update must expire in 150ms even with
                // no further output, or Grok/OpenTUI redraws stay invisible.
                let wait_until = {
                    let state = shared
                        .0
                        .lock()
                        .unwrap_or_else(sync::PoisonError::into_inner);
                    if !state.accepts(epoch) {
                        return Ok(Outcome::Cancelled);
                    }
                    state
                        .view
                        .as_ref()
                        .and_then(snapshot::View::sync_deadline)
                        .unwrap_or_else(|| time::Instant::now() + time::Duration::from_secs(30))
                };
                let notifications = inspector.poll(wait_until)?;
                let mut state = shared
                    .0
                    .lock()
                    .unwrap_or_else(sync::PoisonError::into_inner);
                if !state.accepts(epoch) {
                    return Ok(Outcome::Cancelled);
                }
                let view = state.view.as_mut().expect("view published");
                let seq = view.display_seq();
                let had_output = !notifications.is_empty();
                for notification in notifications {
                    view.apply(notification);
                }
                view.flush_expired_sync();
                let changed = view.display_seq() != seq;
                drop(state);
                // Idle and nudge wakes are not I/O. Stamping last_alive here
                // hid laptop suspend: the worker reset the clock and sat on a
                // dead socket until TCP noticed, instead of reconnecting.
                if had_output {
                    last_alive = reconnect::AliveClock::now();
                }
                if changed {
                    wake();
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Startup {
    ConnectionForm,
    Demo,
}

pub fn run(startup: Startup) -> anyhow::Result<()> {
    window::run(startup)
}

pub fn save_demo(path: &path::Path) -> anyhow::Result<()> {
    let mut state = State {
        view: Some(demo_view()?),
        generation: 1,
        phase: Phase::Demo,
        ..State::default()
    };
    let mut ui = ui::DesktopUi::default();
    ui.form.user = "demo".to_owned();
    window::save_snapshot(&mut state, &mut ui, path)
}

pub(crate) fn home_path() -> Option<path::PathBuf> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(path::PathBuf::from)
}

/// Local account name, used when `User` is omitted from the SSH profile.
/// Same default `ssh` uses: `$USER` / `%USERNAME%`, then `$LOGNAME`.
pub(crate) fn local_user() -> String {
    env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_default()
}

pub(crate) fn demo_view() -> anyhow::Result<snapshot::View> {
    let mut panes = Vec::new();
    for (id, window_id, left, columns, rows) in [(0, 0, 0, 54, 27), (1, 0, 55, 54, 27)] {
        let state = snapshot::State::parse(&format!(
            "%{id}|@{window_id}|{columns}|{rows}|{left}|0|0|0|0|0|2000|||0|{}|1|0|0|0|1|0|0|0|0|0|1|",
            rows - 1
        ))?;
        let mut terminal = terminal::Terminal::new(state.size, 300);
        if id == 0 {
            // Cyan "Starcom" is what the Xvfb smoke test copies.
            terminal.feed(b"\r\n\x1b[36mStarcom\x1b[0m  /  terminal workspace\r\n\r\n");
            terminal
                .feed(b"\x1b[90mThis is built-in demo data, not an SSH session.\x1b[0m\r\n\r\n");
            terminal.feed(b"\x1b[32mdemo@workstation\x1b[0m:~/starcom$ cargo test\r\n\r\n");
            terminal.feed(
                format!(
                    "  \x1b[32mCompiling\x1b[0m starcom v{}\r\n  \x1b[32mFinished\x1b[0m test profile\r\n\r\n",
                    env!("CARGO_PKG_VERSION")
                )
                .as_bytes(),
            );
            for name in [
                "control framing",
                "snapshot -> live",
                "Unicode selection",
                "independent panes",
            ] {
                terminal.feed(format!("test {name:<28} ... \x1b[32mok\x1b[0m\r\n").as_bytes());
            }
            terminal
                .feed(b"\r\n\x1b[36mDrag to select. Selection copies on release.\x1b[0m\r\n\r\n");
            terminal.feed(b"The window is a client; tmux owns your jobs.\r\n\r\n");
            terminal.feed(b"\x1b[32mdemo@workstation\x1b[0m:~/starcom$ ");
        } else {
            for line in 1..=80 {
                terminal.feed(format!("\x1b[90m[12:{:02}:{:02}]\x1b[0m  worker  \x1b[32mready\x1b[0m  batch {line:03}\r\n", line / 60, line % 60).as_bytes());
            }
            terminal.feed(b"\r\n\x1b[33mScroll upward to inspect retained output.\x1b[0m\r\n\r\n");
        }
        panes.push(snapshot::Pane {
            state,
            terminal,
            history_may_be_truncated: false,
        });
    }
    snapshot::View::new(tmuxctl::SessionId(0), panes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_or_replaced_epochs_cannot_publish() {
        let mut state = State::default();
        assert!(state.accepts(0));
        state.cancel();
        assert!(!state.accepts(0));
        assert!(state.accepts(1));
        state.stopping = true;
        assert!(!state.accepts(1));
    }

    #[test]
    fn demo_and_disconnect_need_no_network() {
        let client = Client::new(sync::Arc::new(|| {})).unwrap();
        client.demo().unwrap();
        assert_eq!(client.phase(), Phase::Demo);
        client.with_view(|view| assert_eq!(view.unwrap().panes().len(), 2));
        client.disconnect();
        assert_eq!(client.phase(), Phase::Disconnected);
        client.with_view(|view| assert_eq!(view.unwrap().status(), snapshot::Status::Disconnected));
    }

    fn editable() -> State {
        State::interactive_demo().unwrap()
    }

    #[test]
    fn queue_is_bounded_and_cancellation_invalidates_all_actions() {
        let mut state = editable();
        let target = state.target(tmuxctl::PaneId(0)).unwrap();
        for _ in 0..MAX_PENDING_ACTIONS {
            state
                .enqueue(
                    target,
                    input::Action::Key(input::Key::Enter, input::Modifiers::default()),
                )
                .unwrap();
        }
        assert!(
            state
                .enqueue(target, input::Action::Bytes(b"overflow".to_vec()))
                .is_err()
        );
        assert_eq!(state.actions.len(), MAX_PENDING_ACTIONS);
        state.cancel();
        assert!(state.actions.is_empty());
        assert_eq!(state.action_bytes, 0);
        assert!(
            state
                .enqueue(target, input::Action::Bytes(b"stale".to_vec()))
                .is_err()
        );
    }

    #[test]
    fn cancellation_clears_rename_progress() {
        let client = Client::new(sync::Arc::new(|| {})).unwrap();
        *client.lock() = editable();
        client.lock().renaming = true;
        assert!(client.lock().renaming);
        client.disconnect();
        assert!(!client.lock().renaming);
    }

    #[test]
    fn protocol_teardown_closes_only_a_last_known_pane() {
        let mut state = editable();
        assert!(!ended_last_pane(
            state.view.as_ref(),
            reconnect::Failure::Protocol
        ));
        let keep = *state.view.as_ref().unwrap().panes().keys().next().unwrap();
        state
            .view
            .as_mut()
            .unwrap()
            .panes_mut()
            .retain(|pane, _| *pane == keep);
        assert!(ended_last_pane(
            state.view.as_ref(),
            reconnect::Failure::Protocol
        ));
        assert!(!ended_last_pane(
            state.view.as_ref(),
            reconnect::Failure::Transport
        ));
    }

    #[test]
    fn queue_coalesces_bytes_without_crossing_a_key_or_pane_boundary() {
        let mut state = editable();
        let a = state.target(tmuxctl::PaneId(0)).unwrap();
        let b = state.target(tmuxctl::PaneId(1)).unwrap();
        for bytes in [b"one".to_vec(), b"two".to_vec()] {
            state.enqueue(a, input::Action::Bytes(bytes)).unwrap();
        }
        state
            .enqueue(
                a,
                input::Action::Key(input::Key::Enter, input::Modifiers::default()),
            )
            .unwrap();
        state
            .enqueue(b, input::Action::Bytes(b"three".to_vec()))
            .unwrap();
        assert_eq!(state.actions.len(), 3);
        assert!(
            matches!(&state.actions[0].action, input::Action::Bytes(bytes) if bytes == b"onetwo")
        );
        assert_eq!(state.actions[2].target, b);
        assert_eq!(state.action_bytes, 6 + 32 + 5);
    }

    #[test]
    fn ordinary_input_batches_stop_at_synchronous_barriers() {
        let mut state = editable();
        let target = state.target(tmuxctl::PaneId(0)).unwrap();
        state
            .enqueue(target, input::Action::Bytes(b"hello".to_vec()))
            .unwrap();
        state
            .enqueue(
                target,
                input::Action::Key(input::Key::Enter, input::Modifiers::default()),
            )
            .unwrap();
        state
            .enqueue(
                target,
                input::Action::Paste(input::Paste::new("barrier").unwrap()),
            )
            .unwrap();
        state
            .enqueue(target, input::Action::Bytes(b"later".to_vec()))
            .unwrap();

        let (_, actions, _) = take_ordinary_input(&mut state).unwrap().unwrap();
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], input::Action::Bytes(bytes) if bytes == b"hello"));
        assert!(matches!(
            actions[1],
            input::Action::Key(input::Key::Enter, _)
        ));
        assert!(matches!(state.actions[0].action, input::Action::Paste(_)));
        assert!(matches!(state.actions[1].action, input::Action::Bytes(_)));
        assert_eq!(state.action_bytes, "barrier".len() + "later".len());
    }

    #[test]
    fn input_backlog_counts_queued_and_written_actions() {
        let mut state = editable();
        let target = state.target(tmuxctl::PaneId(0)).unwrap();
        state
            .enqueue(target, input::Action::Bytes(b"one".to_vec()))
            .unwrap();
        state
            .enqueue(target, input::Action::Bytes(b"two".to_vec()))
            .unwrap();
        state
            .enqueue(
                target,
                input::Action::Key(input::Key::Enter, input::Modifiers::default()),
            )
            .unwrap();
        state
            .enqueue(
                target,
                input::Action::Paste(input::Paste::new("not ordinary").unwrap()),
            )
            .unwrap();
        state.inflight_input_actions = 3;

        assert_eq!(
            state.input_backlog(),
            InputBacklog {
                queued: 2,
                in_flight: 3,
            }
        );
        assert_eq!(state.input_backlog().total(), 5);
        state.discard_actions();
        assert_eq!(state.input_backlog(), InputBacklog::default());
    }

    #[test]
    fn queued_rename_is_an_input_pipeline_barrier() {
        let mut state = editable();
        let target = state.target(tmuxctl::PaneId(0)).unwrap();
        state
            .enqueue(target, input::Action::Bytes(b"held".to_vec()))
            .unwrap();
        state.rename = Some((core::SessionName::new("renamed").unwrap(), "old".to_owned()));
        assert!(take_ordinary_input(&mut state).unwrap().is_none());
        assert_eq!(state.actions.len(), 1);
    }

    #[test]
    fn a_gui_batch_is_rejected_as_a_whole_when_it_would_overflow() {
        let client = Client::new(sync::Arc::new(|| {})).unwrap();
        *client.lock() = editable();
        let target = client.target(tmuxctl::PaneId(0)).unwrap();
        let actions = (0..=MAX_PENDING_ACTIONS)
            .map(|_| {
                (
                    target,
                    input::Action::Key(input::Key::Enter, input::Modifiers::default()),
                )
            })
            .collect();
        assert!(client.submit_batch(actions).is_err());
        assert!(client.lock().actions.is_empty());
        assert_eq!(client.lock().action_bytes, 0);
    }
}
