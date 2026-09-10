//! Connection form and terminal workspace for one connection tab.

pub(crate) mod input;
mod layout;
mod terminal;

use std::{collections, fs, path, sync, thread, time};

use crate::{
    core, desktop, input as terminal_input, reconnect, session, snapshot, ssh, ssh_config, store,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Connection,
    Terminal,
}

enum UploadEvent {
    Progress { name: String, done: u64, total: u64 },
    Done(Vec<String>),
    Failed(String),
}

struct Upload {
    events: sync::mpsc::Receiver<UploadEvent>,
    /// The exact pane generation on which the files were dropped. Completion
    /// must never mint a fresh target after a reconnect or layout change.
    target: desktop::Target,
    cancel: sync::Arc<sync::atomic::AtomicBool>,
}

/// A drop that is waiting for Yes/No because a file is over `MAX_FILE_BYTES`.
struct PendingDrop {
    files: Vec<path::PathBuf>,
    label: String,
    target: desktop::Target,
    options: ssh::Options,
    allow_oversize: bool,
}

impl Drop for Upload {
    fn drop(&mut self) {
        self.cancel.store(true, sync::atomic::Ordering::Release);
    }
}

pub struct Form {
    destination: String,
    host: String,
    pub user: String,
    session: String,
    port: u16,
    identity: String,
    identity_files: Vec<String>,
    identities_only: bool,
    host_key_alias: Option<String>,
    jumps: Vec<ssh::Options>,
    known_hosts: String,
    socket: String,
    history: usize,
    interactive: bool,
    reconnect: bool,
    unsupported: Vec<String>,
    profile_error: Option<String>,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            destination: String::new(),
            host: String::new(),
            user: desktop::local_user(),
            session: String::new(),
            port: 22,
            identity: String::new(),
            identity_files: Vec::new(),
            identities_only: false,
            host_key_alias: None,
            jumps: Vec::new(),
            known_hosts: desktop::home_path()
                .map(|path| path.join(".ssh/known_hosts").to_string_lossy().into_owned())
                .unwrap_or_default(),
            socket: String::new(),
            history: store::DEFAULT_HISTORY,
            interactive: true,
            reconnect: true,
            unsupported: Vec::new(),
            profile_error: None,
        }
    }
}

impl Form {
    pub fn destination(&self) -> &str {
        self.destination.trim()
    }

    /// The non-secret half of this form: where to connect and how, never
    /// anything that would let a reader of the saved file connect.
    pub(crate) fn saved(&self) -> store::Tab {
        store::Tab {
            destination: self.destination.clone(),
            host: self.host.clone(),
            user: self.user.clone(),
            session: self.session.clone(),
            window: None,
            pane: None,
            port: self.port,
            identity: self.identity.clone(),
            known_hosts: self.known_hosts.clone(),
            socket: self.socket.clone(),
            history: self.history,
            interactive: self.interactive,
            reconnect: self.reconnect,
        }
    }

    /// Fill a form from a saved tab. The workspace decides whether to reconnect
    /// it after current SSH policy has been applied.
    pub(crate) fn restore(saved: store::Tab) -> Self {
        Self {
            destination: saved.destination,
            host: saved.host,
            user: if saved.user.trim().is_empty() {
                desktop::local_user()
            } else {
                saved.user
            },
            session: saved.session,
            port: saved.port,
            identity: saved.identity,
            identity_files: Vec::new(),
            identities_only: false,
            host_key_alias: None,
            jumps: Vec::new(),
            known_hosts: saved.known_hosts,
            socket: saved.socket,
            history: saved.history.min(snapshot::MAX_HISTORY_LINES),
            interactive: saved.interactive,
            reconnect: saved.reconnect,
            unsupported: Vec::new(),
            profile_error: None,
        }
    }

    fn apply_profile(
        &mut self,
        profile: ssh_config::Profile,
        default_identities: Vec<path::PathBuf>,
    ) {
        self.host.clone_from(&profile.host);
        self.user = profile
            .user
            .clone()
            .filter(|user| !user.is_empty())
            .unwrap_or_else(desktop::local_user);
        self.port = profile.port.unwrap_or(22);
        self.apply_routing(&profile, default_identities);
        self.profile_error = None;
    }

    /// Routing and identity policy from the config, not from the saved tab.
    /// A restored tab must not skip a new IdentityFile or IdentitiesOnly.
    fn apply_routing(
        &mut self,
        profile: &ssh_config::Profile,
        default_identities: Vec<path::PathBuf>,
    ) {
        self.identity_files = profile
            .identities
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if self.identity_files.is_empty() {
            self.identity_files = default_identities
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
        }
        self.identities_only = profile.identities_only;
        self.host_key_alias.clone_from(&profile.host_key_alias);
        if let Some(ref known_hosts) = profile.known_hosts {
            self.known_hosts = known_hosts.to_string_lossy().into_owned();
        }
        self.unsupported.clone_from(&profile.unsupported);
    }

    fn host_ready(&self) -> bool {
        self.profile_error.is_none() && self.unsupported.is_empty() && !self.host.trim().is_empty()
    }

    /// Listing does not attach, so it does not need a session name.
    fn listing(&self) -> anyhow::Result<desktop::Connection> {
        self.connection_named("_")
    }

    fn connection(&self) -> anyhow::Result<desktop::Connection> {
        anyhow::ensure!(!self.session.trim().is_empty(), "choose a tmux session");
        self.connection_named(self.session.trim())
    }

    fn connection_named(&self, session: &str) -> anyhow::Result<desktop::Connection> {
        fn local_path(text: &str) -> anyhow::Result<path::PathBuf> {
            if let Some(tail) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
                return desktop::home_path()
                    .map(|home| home.join(tail))
                    .ok_or_else(|| anyhow::anyhow!("home directory is unavailable"));
            }
            Ok(path::PathBuf::from(text))
        }

        anyhow::ensure!(
            self.unsupported.is_empty(),
            "this SSH profile uses unsupported policy: {}",
            self.unsupported.join(", ")
        );
        anyhow::ensure!(self.profile_error.is_none(), "fix the SSH profile first");
        let mut files = Vec::new();
        if !self.identity.trim().is_empty() {
            files.push(local_path(&self.identity)?);
        }
        for path in &self.identity_files {
            let path = local_path(path)?;
            if !files.contains(&path) {
                files.push(path);
            }
        }
        let authentication = ssh::Authentication {
            files,
            agent: true,
            identities_only: self.identities_only,
        };
        let user = self.user.trim();
        let user = if user.is_empty() {
            desktop::local_user()
        } else {
            user.to_owned()
        };
        let options = ssh::Options {
            host: self.host.trim().to_owned(),
            user,
            port: self.port,
            authentication,
            known_hosts: local_path(&self.known_hosts)?,
            host_key_alias: self.host_key_alias.clone(),
            timeout: time::Duration::from_secs(30),
            jumps: self.jumps.clone(),
        };
        options.validate()?;
        Ok(desktop::Connection {
            options,
            session: core::SessionName::new(session)?,
            socket: (!self.socket.is_empty()).then(|| self.socket.clone()),
            history: self.history,
            access: if self.interactive {
                session::Access::Interactive
            } else {
                session::Access::ReadOnly
            },
            reconnect: self.reconnect,
        })
    }
}

/// One terminal step in the order the user produced it. A clipboard read cannot
/// happen inside the egui closure, so a paste request stays an ordered step
/// rather than replacing the frame's other input.
pub(crate) enum Step {
    Send(desktop::Target, terminal_input::Action),
    RequestPaste(desktop::Target),
}

pub enum Action {
    None,
    Connect(desktop::Connection),
    /// Ask the host which sessions exist. Cannot start a tmux server.
    ListSessions(desktop::Connection),
    /// Explicitly create a session, which may start a server. Only ever from a
    /// confirmed button press, never from a failed attach.
    CreateSession(desktop::Connection),
    Disconnect,
    /// Rename the attached tmux session. The worker updates the tab label
    /// after tmux accepts the name.
    RenameSession(String),
    /// Every terminal step this frame produced, in order. Never a subset: a
    /// frame that cannot deliver all of its steps reports that to the user.
    Frame(Vec<Step>),
    ReloadConfig,
}

pub struct DesktopUi {
    pub form: Form,
    pub config: sync::Arc<ssh_config::Config>,
    pub config_load_error: Option<String>,
    screen: Screen,
    profile_source: String,
    font_size: f32,
    generation: u64,
    windows: collections::BTreeMap<tmuxctl::WindowId, layout::Node>,
    zoomed_windows: collections::BTreeSet<tmuxctl::WindowId>,
    window: Option<tmuxctl::WindowId>,
    /// Last pane the user selected, independent of transient keyboard focus.
    /// It survives tab/window focus changes and is persisted as a resume hint.
    selected: Option<tmuxctl::PaneId>,
    focused: Option<tmuxctl::PaneId>,
    pane_ui: collections::BTreeMap<tmuxctl::PaneId, terminal::PaneUi>,
    notice: Option<String>,
    notice_until: Option<time::Instant>,
    /// Restore keyboard focus the next time this tab's panes are painted.
    restore_focus: bool,
    /// Name for an as-yet-uncreated session, kept apart from the selected one
    /// so typing it cannot be overwritten by the live listing.
    create_name: String,
    /// Session we just asked to create; shown in the list and attached as soon
    /// as the host confirms it.
    creating: Option<String>,
    /// In-flight SFTP upload from a file drop. The control worker is not
    /// involved; progress and completion are polled on the next frames.
    upload: Option<Upload>,
    upload_progress: Option<(String, u64, u64)>,
    drop_prompt: Option<PendingDrop>,
    /// Whether a local SSH agent looked reachable, and when that was last
    /// asked. A hint for the form only; the connection still authenticates as
    /// configured. Rechecked while the form is open so that starting an agent
    /// clears the warning without restarting Starcom.
    agent_available: bool,
    agent_checked: time::Instant,
    /// Destination we last asked the host to list sessions for. Empty means
    /// never listed. Restored tabs copy the destination here because startup
    /// attaches directly instead of performing a redundant discovery request.
    listed_destination: String,
    /// List as soon as a host is chosen or a custom destination is committed.
    auto_list: bool,
    /// Last cell size sent to tmux, so we do not spam refresh-client -C.
    client_cells: Option<core::Size>,
    pending_client_cells: Option<(core::Size, time::Instant)>,
    /// The status marker advances once per visible terminal refresh, not with
    /// wall time. An idle tab therefore cannot animate itself.
    refresh_tick: u64,
    last_refresh: Option<(u64, u64)>,
}

impl Default for DesktopUi {
    fn default() -> Self {
        Self::with_config(sync::Arc::new(ssh_config::Config::default()), None)
    }
}

impl DesktopUi {
    pub fn with_config(
        config: sync::Arc<ssh_config::Config>,
        config_load_error: Option<String>,
    ) -> Self {
        Self {
            form: Form::default(),
            config,
            config_load_error,
            screen: Screen::Connection,
            profile_source: String::new(),
            font_size: 14.0,
            generation: u64::MAX,
            windows: collections::BTreeMap::new(),
            zoomed_windows: collections::BTreeSet::new(),
            window: None,
            selected: None,
            focused: None,
            pane_ui: collections::BTreeMap::new(),
            notice: None,
            notice_until: None,
            restore_focus: false,
            create_name: String::new(),
            creating: None,
            upload: None,
            upload_progress: None,
            drop_prompt: None,
            agent_available: ssh::agent_available(),
            agent_checked: time::Instant::now(),
            listed_destination: String::new(),
            auto_list: false,
            client_cells: None,
            pending_client_cells: None,
            refresh_tick: 0,
            last_refresh: None,
        }
    }

    /// Whether an SSH agent looks reachable, re-asked at most once a second.
    ///
    /// The form is drawn every frame and the probe touches the filesystem, so
    /// it is throttled; a second of staleness on a warning label is invisible,
    /// while never re-asking would leave the warning up after the user starts
    /// an agent.
    fn agent_probe(&mut self) -> bool {
        const INTERVAL: time::Duration = time::Duration::from_secs(1);
        let now = time::Instant::now();
        if now.duration_since(self.agent_checked) >= INTERVAL {
            self.agent_available = ssh::agent_available();
            self.agent_checked = now;
        }
        self.agent_available
    }

    /// Restore a saved tab into this UI's form using current SSH policy.
    pub(crate) fn restore(&mut self, saved: store::Tab) {
        self.window = saved.window.map(tmuxctl::WindowId);
        self.selected = saved.pane.map(tmuxctl::PaneId);
        self.focused = None;
        self.form = Form::restore(saved);
        self.profile_source = self.form.destination().to_owned();
        // Restoring a tab must not contact the host. Pretend we already listed
        // so the form does not fire SSH on first paint.
        self.listed_destination = self.form.destination().to_owned();
        self.auto_list = false;
        self.screen = Screen::Connection;
        if !self.profile_source.is_empty() {
            match self.config.resolve(&self.profile_source) {
                Ok(profile) => self
                    .form
                    .apply_routing(&profile, self.config.default_identities()),
                Err(error) => self.form.profile_error = Some(error.to_string()),
            }
            self.refresh_route();
        }
    }

    /// Build the saved attachment and show its terminal while the worker
    /// reconnects. Invalid or incomplete saved forms stay on the connection
    /// screen so the user can repair them.
    pub(crate) fn resume(&mut self) -> anyhow::Result<desktop::Connection> {
        let connection = self.form.connection()?;
        self.open_terminal();
        Ok(connection)
    }

    pub(crate) fn saved(&self) -> store::Tab {
        let mut saved = self.form.saved();
        saved.window = self.window.map(|window| window.0);
        saved.pane = self.focused.or(self.selected).map(|pane| pane.0);
        saved
    }

    pub fn open_terminal(&mut self) {
        self.screen = Screen::Terminal;
    }

    #[cfg(test)]
    pub(crate) fn showing_form(&self) -> bool {
        self.screen == Screen::Connection
    }

    /// Show this tab's connection form. Exit uses this; the form fields stay.
    pub(crate) fn return_to_form(&mut self) {
        self.screen = Screen::Connection;
        self.cancel_transient();
    }

    pub(crate) fn reset_client_size(&mut self) {
        self.client_cells = None;
        self.pending_client_cells = None;
    }

    pub fn cancel_transient(&mut self) {
        self.creating = None;
        self.upload = None;
        self.upload_progress = None;
        self.drop_prompt = None;
    }

    pub(crate) fn session_name(&self) -> &str {
        self.form.session.trim()
    }

    pub(crate) fn set_session_name(&mut self, name: String) {
        self.form.session = name;
    }

    pub(crate) fn arm_focus_restore(&mut self) {
        self.restore_focus = true;
    }

    pub fn refresh_profile(&mut self) {
        let destination = self.form.destination().to_owned();
        self.profile_source = destination.clone();
        self.form.unsupported.clear();
        self.form.profile_error = None;
        self.form.jumps.clear();
        if destination.is_empty() {
            self.form.host.clear();
            self.form.identity_files.clear();
            self.form.identities_only = false;
            self.form.host_key_alias = None;
            return;
        }
        match self.config.resolve(&destination) {
            Ok(profile) => self
                .form
                .apply_profile(profile, self.config.default_identities()),
            Err(error) => {
                self.form.host = destination;
                self.form.profile_error = Some(error.to_string());
            }
        }
        self.refresh_route();
    }

    fn refresh_route(&mut self) {
        self.form.jumps.clear();
        if self.form.profile_error.is_some() || !self.form.unsupported.is_empty() {
            return;
        }
        match self.config.connection(
            self.form.destination(),
            &desktop::local_user(),
            time::Duration::from_secs(30),
        ) {
            Ok(options) => self.form.jumps = options.jumps,
            Err(error) => self.form.profile_error = Some(error.to_string()),
        }
    }

    pub fn terminal_focused(&self, ctx: &egui::Context) -> bool {
        self.screen == Screen::Terminal
            && self.focused.is_some_and(|pane| {
                ctx.memory(|memory| memory.has_focus(terminal::focus_id(self.generation, pane)))
            })
    }

    /// Resolve one clipboard read into an ordered action. Returning None never
    /// means "silently dropped": every path that declines to send leaves a
    /// notice for the user.
    pub(crate) fn clipboard_paste(
        &mut self,
        state: &desktop::State,
        target: desktop::Target,
        text: &str,
    ) -> Option<terminal_input::Action> {
        if state.target(target.pane()) != Some(target) {
            self.notice = Some("Paste target changed; nothing was sent.".to_owned());
            return None;
        }
        match terminal_input::Paste::new(text) {
            Ok(paste) => Some(terminal_input::Action::Paste(paste)),
            Err(error) => {
                self.notice = Some(error.to_string());
                None
            }
        }
    }

    pub fn show(&mut self, root: &mut egui::Ui, state: &mut desktop::State) -> Action {
        if self.profile_source != self.form.destination() {
            self.refresh_profile();
        }
        // The workspace replaces the form once a view exists, not when Connect
        // is pressed. A failed first attach has nothing to show there.
        if state.view.is_some()
            && self.screen == Screen::Connection
            && self.generation != state.generation
        {
            self.open_terminal();
        } else if self.screen == Screen::Terminal
            && state.view.is_none()
            && matches!(
                state.phase,
                desktop::Phase::Failed | desktop::Phase::Disconnected
            )
        {
            self.screen = Screen::Connection;
        }
        // The control session ended on purpose: last pane `exit`, an explicit
        // detach, or tmux itself going away. Same as the Exit button. Transport
        // loss still reconnects; that is the only automatic retry.
        if self.screen == Screen::Terminal
            && matches!(
                state.failure,
                Some(
                    reconnect::Failure::Detached
                        | reconnect::Failure::MissingSession
                        | reconnect::Failure::ServerExit
                )
            )
            && matches!(
                state.phase,
                desktop::Phase::Disconnected | desktop::Phase::Failed
            )
        {
            return Action::Disconnect;
        }
        match self.screen {
            Screen::Connection => self.show_connection(root, state),
            Screen::Terminal => self.show_terminal(root, state),
        }
    }

    fn show_connection(&mut self, root: &mut egui::Ui, state: &desktop::State) -> Action {
        let mut action = Action::None;
        let mut chosen = None;
        egui::CentralPanel::default().show_inside(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.heading("Connect");
                    ui.weak("One Starcom tab is one tmux session on one host.");
                });
                ui.add_space(14.0);
                ui.scope(|ui| {
                    ui.set_max_width(680.0);
                    let idle = !matches!(
                        state.phase,
                        desktop::Phase::Connecting
                            | desktop::Phase::Watching
                            | desktop::Phase::Resynchronizing
                            | desktop::Phase::Reconnecting
                    );
                    let busy = matches!(state.discovery, Some(desktop::Discovery::Running));
                    let host_ready = self.form.host_ready();
                    let listing_here = self.listed_destination == self.form.destination();

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Host").size(18.0).strong());
                        if ui.small_button("Reload config").clicked() {
                            action = Action::ReloadConfig;
                        }
                    });
                    ui.add_space(6.0);
                    let aliases: Vec<String> =
                        self.config.aliases().iter().take(32).cloned().collect();
                    let mut focus_destination = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                        ui.spacing_mut().button_padding = egui::vec2(16.0, 10.0);
                        if aliases.is_empty() {
                            ui.weak("No literal Host entries in ~/.ssh/config.");
                        }
                        for alias in &aliases {
                            let selected = self.form.destination() == alias;
                            let text = egui::RichText::new(alias).size(22.0).strong();
                            if ui
                                .add(
                                    egui::Button::new(text)
                                        .selected(selected)
                                        .min_size(egui::vec2(0.0, 44.0))
                                        .corner_radius(8.0)
                                        .sense(egui::Sense::CLICK),
                                )
                                .clicked()
                            {
                                if selected {
                                    self.listed_destination.clear();
                                }
                                self.form.destination = alias.clone();
                                self.refresh_profile();
                                self.auto_list = true;
                                focus_destination = true;
                            }
                        }
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.form.destination)
                                .id(egui::Id::new("starcom-destination"))
                                .font(egui::FontId::proportional(22.0))
                                .margin(egui::Margin::symmetric(16, 10))
                                .desired_width(240.0)
                                .min_size(egui::vec2(240.0, 44.0))
                                .hint_text("hostname, address, or alias"),
                        );
                        if focus_destination {
                            response.request_focus();
                        }
                        if response.changed() {
                            self.refresh_profile();
                            self.auto_list = false;
                        }
                        if response.lost_focus()
                            && (ui.input(|input| input.key_pressed(egui::Key::Enter))
                                || !self.form.destination().is_empty())
                        {
                            self.auto_list = true;
                        }
                    });

                    if let Some(ref error) = self.config_load_error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    if let Some(ref error) = self.form.profile_error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    if !self.form.unsupported.is_empty() {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!(
                                "Unsupported SSH policy: {}. Starcom will not silently bypass it.",
                                self.form.unsupported.join(", ")
                            ),
                        );
                    }
                    if !self.form.host.is_empty() {
                        let endpoint = format!(
                            "{}@{}:{}",
                            self.form.user, self.form.host, self.form.port
                        );
                        ui.weak(match self.form.host_key_alias {
                            Some(ref alias) => format!("{endpoint} (host key {alias})"),
                            None => endpoint,
                        });
                    }

                    if !self.form.jumps.is_empty() {
                        let route = self.form.jumps.iter()
                            .map(|hop| format!("{}@{}:{}", hop.user, hop.host, hop.port))
                            .collect::<Vec<_>>().join(" → ");
                        ui.weak(format!("Via {route}"));
                    }

                    ui.add_space(12.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Session").strong());
                        if ui
                            .add_enabled(
                                host_ready && idle && !busy,
                                egui::Button::new("Refresh"),
                            )
                            .on_hover_text(
                                "Ask the host which tmux sessions exist. Uses tmux -N, so it \
                                 cannot start a server.",
                            )
                            .clicked()
                        {
                            self.listed_destination.clear();
                            self.auto_list = true;
                        }
                        match state.discovery {
                            None if self.form.destination().is_empty() => {
                                ui.weak("Pick a host to see its sessions.");
                            }
                            None => {
                                ui.weak("Sessions appear after the host is contacted.");
                            }
                            Some(desktop::Discovery::Running) => {
                                ui.weak("Asking the host…");
                            }
                            Some(desktop::Discovery::Failed(ref detail)) if listing_here => {
                                self.creating = None;
                                ui.colored_label(ui.visuals().error_fg_color, detail);
                            }
                            Some(desktop::Discovery::Created(ref name)) if listing_here => {
                                chosen = Some(name.clone());
                                if self.creating.as_deref() == Some(name.as_str()) {
                                    self.creating = None;
                                    self.create_name.clear();
                                    self.form.session.clone_from(name);
                                    match self.form.connection() {
                                        Ok(connection) => {
                                            self.notice = None;
                                            action = Action::Connect(connection);
                                        }
                                        Err(error) => self.notice = Some(error.to_string()),
                                    }
                                }
                            }
                            Some(desktop::Discovery::Sessions(ref found)) if listing_here => {
                                let pending = self.creating.as_ref().filter(|name| {
                                    !found.iter().any(|summary| summary.name == **name)
                                });
                                if found.is_empty() && pending.is_none() {
                                    ui.weak("The host is running tmux with no sessions.");
                                } else {
                                    if !found
                                        .iter()
                                        .any(|summary| summary.name == self.form.session)
                                        && pending.is_none()
                                    {
                                        chosen = Some(found[0].name.clone());
                                    }
                                    if let Some(name) = pending {
                                        let selected = self.form.session == *name;
                                        ui.add(
                                            egui::Button::new(name)
                                                .selected(selected)
                                                .sense(egui::Sense::CLICK),
                                        );
                                    }
                                    for summary in found {
                                        let selected = self.form.session == summary.name;
                                        let mut text = summary.name.clone();
                                        if summary.attached > 0 {
                                            text.push_str(" · attached");
                                        }
                                        let response = ui.add(
                                            egui::Button::new(text)
                                                .selected(selected)
                                                .sense(egui::Sense::CLICK),
                                        );
                                        if response.clicked() {
                                            chosen = Some(summary.name.clone());
                                        }
                                        if response.double_clicked() && idle && !busy {
                                            chosen = Some(summary.name.clone());
                                            self.form.session = summary.name.clone();
                                            match self.form.connection() {
                                                Ok(connection) => {
                                                    self.notice = None;
                                                    action = Action::Connect(connection);
                                                }
                                                Err(error) => {
                                                    self.notice = Some(error.to_string())
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Some(_) => {
                                ui.weak("Sessions appear after the host is contacted.");
                            }
                        }
                        if host_ready {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.create_name)
                                    .hint_text("new session")
                                    .desired_width(160.0)
                                    .min_size(egui::vec2(160.0, 28.0)),
                            );
                            if ui
                                .add_enabled(
                                    idle
                                        && !busy
                                        && !self.create_name.trim().is_empty(),
                                    egui::Button::new("Create").sense(egui::Sense::CLICK),
                                )
                                .on_hover_text(
                                    "Create a named session on the host. This starts a tmux \
                                     server if none is running.",
                                )
                                .clicked()
                            {
                                match self.form.connection_named(self.create_name.trim()) {
                                    Ok(connection) => {
                                        self.notice = None;
                                        self.creating =
                                            Some(self.create_name.trim().to_owned());
                                        self.form.session = self.create_name.trim().to_owned();
                                        action = Action::CreateSession(connection);
                                    }
                                    Err(error) => self.notice = Some(error.to_string()),
                                }
                            }
                        }
                    });

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let connecting = state.phase == desktop::Phase::Connecting;
                        let can_connect =
                            host_ready && !self.form.session.trim().is_empty() && idle && !connecting;
                        let connect_label = if connecting {
                            "   Connecting…"
                        } else {
                            "Connect"
                        };
                        if connecting {
                            ui.ctx()
                                .request_repaint_after(time::Duration::from_millis(50));
                        }
                        let connect = ui.add_enabled(
                            can_connect || connecting,
                            egui::Button::new(connect_label)
                                .selected(connecting)
                                .min_size(egui::vec2(112.0, 28.0))
                                .sense(egui::Sense::CLICK),
                        );
                        if connecting {
                            let indicator = egui::Rect::from_center_size(
                                egui::pos2(
                                    connect.rect.left() + 14.0,
                                    connect.rect.center().y,
                                ),
                                egui::vec2(14.0, 14.0),
                            );
                            paint_activity_indicator(ui, indicator, ui.ctx().time());
                        }
                        if connect.clicked() && !connecting {
                            match self.form.connection() {
                                Ok(connection) => {
                                    self.notice = None;
                                    action = Action::Connect(connection);
                                }
                                Err(error) => self.notice = Some(error.to_string()),
                            }
                        }
                        if matches!(
                            state.phase,
                            desktop::Phase::Connecting
                                | desktop::Phase::Watching
                                | desktop::Phase::Resynchronizing
                        ) && ui.button("Back to terminal").clicked()
                        {
                            self.open_terminal();
                        }
                    });
                    if let Some(ref error) = state.error {
                        ui.add_space(6.0);
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }

                    ui.add_space(10.0);
                    ui.checkbox(&mut self.form.interactive, "Allow terminal input");
                    ui.checkbox(
                        &mut self.form.reconnect,
                        "Reconnect automatically after connection loss",
                    )
                    .on_hover_text(
                        "Only transport loss is retried. Authentication, host-key, \
                         missing-session and detach failures always stop and wait for you.",
                    );
                    let mut keys = Vec::new();
                    if !self.form.identity.trim().is_empty() {
                        keys.push(self.form.identity.trim().to_owned());
                    }
                    for path in &self.form.identity_files {
                        if !keys.iter().any(|shown| shown == path) {
                            keys.push(path.clone());
                        }
                    }
                    if !keys.is_empty() {
                        ui.weak(format!("Keys: {}", keys.join(", ")));
                    }
                    if self.form.identities_only {
                        ui.weak("IdentitiesOnly: the SSH agent will not be used.");
                    } else if !self.agent_probe() && self.form.identity_files.is_empty() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "No SSH agent found and no IdentityFile. Start an agent \
                             and add a key, or add IdentityFile to ~/.ssh/config.",
                        );
                    }
                    ui.collapsing("Advanced", |ui| {
                        field(ui, "Extra identity file", &mut self.form.identity);
                        field(ui, "User", &mut self.form.user);
                        field(ui, "Host name / address", &mut self.form.host);
                        field(ui, "Known hosts", &mut self.form.known_hosts);
                        field(
                            ui,
                            "Remote tmux socket (optional)",
                            &mut self.form.socket,
                        );
                        ui.horizontal(|ui| {
                            ui.label("Port");
                            ui.add(egui::DragValue::new(&mut self.form.port).range(1..=65535));
                        });
                        ui.horizontal(|ui| {
                            ui.label("History lines");
                            ui.add(
                                egui::DragValue::new(&mut self.form.history)
                                    .range(0..=snapshot::MAX_HISTORY_LINES),
                            );
                        });
                    });
                    if let Some(ref notice) = self.notice {
                        ui.colored_label(ui.visuals().warn_fg_color, notice);
                    }
                    ui.add_space(8.0);
                    ui.weak("The remote host needs only stock SSH and tmux. Host keys are never accepted automatically.");
                });
            });
        });
        if let Some(name) = chosen {
            self.form.session = name;
        }
        if matches!(action, Action::None) && self.auto_list {
            self.auto_list = false;
            let dest = self.form.destination().to_owned();
            let idle = !matches!(
                state.phase,
                desktop::Phase::Connecting
                    | desktop::Phase::Watching
                    | desktop::Phase::Resynchronizing
                    | desktop::Phase::Reconnecting
            );
            let busy = matches!(state.discovery, Some(desktop::Discovery::Running));
            if !dest.is_empty()
                && dest != self.listed_destination
                && self.form.host_ready()
                && idle
                && !busy
            {
                self.listed_destination = dest;
                match self.form.listing() {
                    Ok(connection) => {
                        self.notice = None;
                        action = Action::ListSessions(connection);
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
        }
        action
    }

    fn rebuild_layout(&mut self, state: &desktop::State) {
        if self.generation == state.generation {
            return;
        }
        let preferred = self.focused.or(self.selected);
        self.windows.clear();
        self.zoomed_windows.clear();
        self.focused = None;
        if let Some(ref view) = state.view {
            let mut grouped = collections::BTreeMap::<_, Vec<_>>::new();
            for pane in view.panes().values() {
                grouped.entry(pane.state.window).or_default().push(pane);
            }
            // Follow a saved pane if it moved to another window. If it no
            // longer exists, keep its old window when possible and choose a
            // real pane there; otherwise fall back to the first live window.
            let preferred_window = preferred.and_then(|wanted| {
                grouped.iter().find_map(|(window, panes)| {
                    panes
                        .iter()
                        .any(|pane| pane.state.pane == wanted)
                        .then_some(*window)
                })
            });
            let id = preferred_window
                .or_else(|| self.window.filter(|id| grouped.contains_key(id)))
                .or_else(|| grouped.keys().next().copied());
            if let Some(id) = id
                && let Some(panes) = grouped.get(&id)
            {
                if let Some(node) = layout::Node::from_panes_or_zoom(panes) {
                    if layout::Node::is_zoomed(panes) {
                        self.zoomed_windows.insert(id);
                    }
                    let visible = node.pane_ids();
                    self.focused = preferred
                        .filter(|pane| visible.contains(pane))
                        .or_else(|| visible.first().copied());
                    self.selected = self.focused;
                    self.windows.insert(id, node);
                } else {
                    self.selected = None;
                    self.notice =
                        Some(format!("Cannot reconstruct the pane layout in window {id}"));
                }
                self.window = Some(id);
            } else {
                self.window = None;
                self.selected = None;
            }
        }
        self.pane_ui.retain(|id, pane_ui| {
            let Some(view) = state.view.as_ref() else {
                return false;
            };
            if !view.panes().contains_key(id) {
                return false;
            }
            pane_ui.on_layout_rebuild();
            true
        });
        self.generation = state.generation;
    }

    fn note_refresh(&mut self, state: &desktop::State, scrolled: bool) {
        let refresh = state
            .view
            .as_ref()
            .map(|view| (state.generation, view.display_seq()));
        if refresh != self.last_refresh || scrolled {
            self.refresh_tick = self.refresh_tick.wrapping_add(1);
            self.last_refresh = refresh;
        }
    }

    fn show_terminal(&mut self, root: &mut egui::Ui, state: &mut desktop::State) -> Action {
        let scrolled = root.input(|input| input.smooth_scroll_delta != egui::Vec2::ZERO);
        self.note_refresh(state, scrolled);
        let generation_changed = self.generation != state.generation;
        self.rebuild_layout(state);
        if self.restore_focus && self.focused.is_none() {
            self.focused = self.selected;
        }
        if (generation_changed || self.restore_focus)
            && let Some(pane) = self.focused
        {
            root.ctx().memory_mut(|memory| {
                memory.request_focus(terminal::focus_id(self.generation, pane))
            });
        }
        self.restore_focus = false;
        if let Some(until) = self.notice_until
            && until <= time::Instant::now()
        {
            self.notice = None;
            self.notice_until = None;
        } else if let Some(until) = self.notice_until {
            root.ctx()
                .request_repaint_after(until.saturating_duration_since(time::Instant::now()));
        }
        // Navigation and terminal steps are separate results, so a button press
        // can never quietly consume the keystrokes collected in the same frame.
        let mut action = Action::None;
        let mut steps: Vec<Step> = Vec::new();
        let connection_epoch = state.epoch();
        if let Some((target, paths)) = self.poll_upload(root.ctx()) {
            let n = paths.len();
            let label = if n == 1 { "file" } else { "files" };
            if state.target(target.pane()) == Some(target) {
                self.notice = Some(format!("Uploaded {n} {label}."));
                self.notice_until = Some(time::Instant::now() + time::Duration::from_secs(3));
                match paste_remote_paths(&paths)
                    .and_then(|text| terminal_input::Paste::new(&text).map_err(anyhow::Error::from))
                {
                    Ok(paste) => {
                        steps.push(Step::Send(target, terminal_input::Action::Paste(paste)))
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            } else {
                self.notice = Some(format!(
                    "Uploaded {n} {label}, but the original pane changed; paths were not pasted."
                ));
                self.notice_until = Some(time::Instant::now() + time::Duration::from_secs(5));
            }
        }

        // egui remembers last frame's panel rect with no max. A single wrap
        // to a tall status bar then never shrinks, which is the "status ate
        // the window" failure after a long session.
        egui::Panel::bottom("status")
            .resizable(false)
            .exact_size(36.0_f32)
            .show_inside(root, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Exit").size(14.0))
                            .min_size(egui::vec2(0.0, 28.0))
                            .corner_radius(5.0)
                            .sense(egui::Sense::CLICK),
                    )
                    .on_hover_text(
                        "Drop this attachment and return to the connection form. \
                         Remote jobs keep running.",
                    )
                    .clicked()
                {
                    action = Action::Disconnect;
                    self.return_to_form();
                }
                if let Some(pane) = self.focused.and_then(|id| {
                    state.view.as_ref().and_then(|view| view.panes().get(&id))
                }) {
                    let size = pane.terminal.size();
                    ui.small(format!("{}×{}", size.columns(), size.rows()));
                }
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        egui::Frame::NONE
                            .fill(ui.visuals().code_bg_color)
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(6, 2))
                            .show(ui, |ui| {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(14.0, 14.0),
                                    egui::Sense::hover(),
                                );
                                paint_refresh_indicator(ui, rect, self.refresh_tick);
                            });
                        if matches!(
                            state.phase,
                            desktop::Phase::Connecting
                                | desktop::Phase::Reconnecting
                                | desktop::Phase::Resynchronizing
                        ) {
                            ui.ctx()
                                .request_repaint_after(time::Duration::from_millis(50));
                        }
                        if let Some(rtt) = state.last_rtt {
                            ui.separator();
                            ui.small(if rtt.as_millis() == 0 {
                                "<1 ms".to_owned()
                            } else {
                                format!("{} ms", rtt.as_millis())
                            });
                        }
                        ui.separator();
                        if click_button(ui, "−")
                            .on_hover_text("Smaller terminal text")
                            .clicked()
                        {
                            self.font_size = (self.font_size - 1.0).max(10.0);
                            self.reset_client_size();
                        }
                        ui.small(format!("{} pt", self.font_size as u32));
                        if click_button(ui, "+")
                            .on_hover_text("Larger terminal text")
                            .clicked()
                        {
                            self.font_size = (self.font_size + 1.0).min(28.0);
                            self.reset_client_size();
                        }
                        if click_button(ui, "Copy")
                            .on_hover_text("Copy the whole pane")
                            .clicked()
                        {
                            self.copy_pane(ui.ctx(), state, true);
                        }
                        if ui
                            .add_enabled(
                                state.input_ready(),
                                egui::Button::new("Paste").sense(egui::Sense::CLICK),
                            )
                            .clicked()
                            && let Some(target) = self.focused.and_then(|pane| state.target(pane))
                        {
                            steps.push(Step::RequestPaste(target));
                        }
                        ui.separator();
                        if let Some((ref name, done, total)) = self.upload_progress {
                            let frac = if total == 0 {
                                1.0
                            } else {
                                done as f32 / total as f32
                            };
                            ui.add(
                                egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                                    .desired_width(200.0)
                                    .desired_height(16.0)
                                    .text(upload_label(name, done, total)),
                            );
                            if click_button(ui, "Cancel")
                                .on_hover_text("Stop this upload. Partial remote files are removed.")
                                .clicked()
                            {
                                self.upload = None;
                                self.upload_progress = None;
                                self.notice = Some("Upload cancelled.".to_owned());
                                self.notice_until = None;
                            }
                        } else if let Some(prompt) = self.drop_prompt.as_ref() {
                            ui.small(&prompt.label);
                            if click_button(ui, "Yes").clicked() {
                                if let Some(pending) = self.drop_prompt.take() {
                                    self.start_upload(state, pending);
                                }
                            } else if click_button(ui, "No").clicked() {
                                self.drop_prompt = None;
                            }
                        } else if self.notice.as_deref() == Some("Copied!") {
                            ui.label(
                                egui::RichText::new("Copied!")
                                    .color(egui::Color32::WHITE)
                                    .strong(),
                            );
                        } else {
                            ui.small(if let Some(ref notice) = self.notice {
                                notice.as_str()
                            } else if state.phase == desktop::Phase::Demo {
                                "Local demo — no SSH connection"
                            } else if state.input_ready() {
                                "Click a pane to type · Drag to select · Drop files to upload"
                            } else {
                                "Wheel to scroll · Drag to select"
                            });
                        }
                        if let Some(retry) = state.retry {
                            // The countdown is the only thing on screen that changes on
                            // its own, so ask for exactly the frames it needs.
                            ui.ctx().request_repaint_after(
                                retry.remaining().min(time::Duration::from_millis(250)),
                            );
                            ui.separator();
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "Reconnecting: attempt {} in {:.0}s. Nothing you type now is queued.",
                                        retry.attempt,
                                        retry.remaining().as_secs_f32().ceil()
                                    ))
                                    .color(ui.visuals().warn_fg_color)
                                    .small(),
                                )
                                .truncate(),
                            );
                        }
                        if let Some(ref continuity) = state.continuity {
                            ui.separator();
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(continuity.as_str())
                                        .color(ui.visuals().warn_fg_color)
                                        .small(),
                                )
                                .truncate(),
                            );
                        }
                        if let Some(ref error) = state.error {
                            ui.separator();
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(error.as_str())
                                        .color(ui.visuals().error_fg_color)
                                        .small(),
                                )
                                .truncate(),
                            );
                        }
                    },
                );
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show_inside(root, |ui| {
                if state.view.is_none() {
                    ui.vertical_centered(|ui| {
                        ui.set_max_width(640.0);
                        ui.add_space(90.0);
                        ui.heading(state.phase.label());
                        if let Some(ref error) = state.error {
                            ui.add_space(8.0);
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                        ui.add_space(8.0);
                        ui.label("The connection form is kept in this tab.");
                    });
                    return;
                }
                let rect = ui.available_rect_before_wrap();
                let (cell_width, row_height) = terminal::cell_metrics(ui, self.font_size);
                let controls = state.access == session::Access::Interactive && state.input_ready();
                if state.input_ready() {
                    let inner = rect.shrink(4.0);
                    let columns = ((inner.width() / cell_width).floor() as usize).max(1);
                    let rows = ((inner.height() / row_height).floor() as usize).max(1);
                    if let Ok(size) = core::Size::new(columns, rows)
                        && self.client_cells != Some(size)
                    {
                        let stable = match self.pending_client_cells {
                            Some((pending, at)) if pending == size => {
                                at.elapsed() >= time::Duration::from_millis(200)
                            }
                            _ => {
                                self.pending_client_cells = Some((size, time::Instant::now()));
                                false
                            }
                        };
                        if !stable {
                            ui.ctx()
                                .request_repaint_after(time::Duration::from_millis(200));
                        } else {
                            let window = self.window;
                            let pane = self.focused.or_else(|| {
                                state.view.as_ref().and_then(|view| {
                                    view.panes().values().find_map(|pane| {
                                        (Some(pane.state.window) == window)
                                            .then_some(pane.state.pane)
                                    })
                                })
                            });
                            if let Some(pane) = pane
                                && let Some(target) = state.target(pane)
                            {
                                self.client_cells = Some(size);
                                self.pending_client_cells = None;
                                steps.push(Step::Send(
                                    target,
                                    terminal_input::Action::ClientSize(size),
                                ));
                            }
                        }
                    } else {
                        self.pending_client_cells = None;
                    }
                }
                if let (Some(id), Some(view)) = (self.window, state.view.as_mut())
                    && let Some(node) = self.windows.get_mut(&id)
                {
                    let generation = self.generation;
                    let font_size = self.font_size;
                    let pane_ui = &mut self.pane_ui;
                    let zoomed = self.zoomed_windows.contains(&id);
                    let focused = &mut self.focused;
                    let notice = &mut self.notice;
                    let notice_until = &mut self.notice_until;
                    let mut resizes = Vec::new();
                    let mut pane_events = Vec::new();
                    let can_kill = view
                        .panes()
                        .values()
                        .filter(|pane| pane.state.window == id)
                        .count()
                        > 1;
                    node.draw(
                        ui,
                        rect,
                        egui::Id::new(("split", id.0, generation, connection_epoch)),
                        controls,
                        cell_width,
                        row_height,
                        &mut resizes,
                        &mut |ui, rect, pane_id| {
                            let neighbors = layout::Neighbors::of(
                                view.panes()
                                    .values()
                                    .map(|pane| layout::PaneRect::from_state(&pane.state)),
                                pane_id,
                            );
                            if let Some(pane) = view.panes_mut().get_mut(&pane_id) {
                                let events = pane_ui.entry(pane_id).or_default().show(
                                    ui,
                                    rect,
                                    pane,
                                    generation,
                                    font_size,
                                    focused,
                                    notice,
                                    notice_until,
                                    controls,
                                    can_kill,
                                    neighbors,
                                    zoomed,
                                    !matches!(
                                        state.phase,
                                        desktop::Phase::Watching | desktop::Phase::Demo
                                    ),
                                );
                                pane_events
                                    .extend(events.into_iter().map(|action| (pane_id, action)));
                            }
                        },
                    );
                    for (pane, resize) in resizes {
                        if let Some(target) = state.target(pane) {
                            steps.push(Step::Send(target, terminal_input::Action::Resize(resize)));
                        }
                    }
                    for (pane, action) in pane_events {
                        if let Some(target) = state.target(pane) {
                            steps.push(Step::Send(target, action));
                        }
                    }
                }
            });

        // PaneUi clears keyboard focus when the app or another widget takes
        // it. Keep the last positive selection separately so shutdown and tab
        // switching do not forget which pane the user chose.
        if let Some(pane) = self.focused {
            self.selected = Some(pane);
        }

        if matches!(action, Action::None) && self.terminal_focused(root.ctx()) {
            let (events, modifiers) = root.input(|input| (input.events.clone(), input.modifiers));
            let target = self.focused.and_then(|pane| state.target(pane));
            if let Some(target) = target {
                for event in &events {
                    match input::translate(event, modifiers) {
                        Ok(Some(input::Event::Input(actions))) => {
                            steps.extend(
                                actions.into_iter().map(|action| Step::Send(target, action)),
                            );
                        }
                        Ok(Some(input::Event::Copy)) => self.copy_pane(root.ctx(), state, false),
                        Ok(Some(input::Event::Paste(paste))) => {
                            steps.push(Step::Send(target, terminal_input::Action::Paste(paste)));
                        }
                        // Keeps its place in the frame; the clipboard read that
                        // resolves it happens outside this closure.
                        Ok(Some(input::Event::RequestPaste)) => {
                            steps.push(Step::RequestPaste(target))
                        }
                        Ok(None) => {}
                        Err(error) => self.notice = Some(error.to_string()),
                    }
                }
            }
        }

        if matches!(action, Action::None) && !steps.is_empty() {
            Action::Frame(steps)
        } else {
            if !steps.is_empty() {
                // Only reachable if a navigation button and a terminal step land
                // in one frame. Say so rather than discarding input in silence.
                self.notice = Some(
                    "Terminal input was not sent because this frame changed the connection instead. Nothing was retried."
                        .to_owned(),
                );
            }
            action
        }
    }

    pub(crate) fn take_dropped_files(&mut self, ctx: &egui::Context, state: &desktop::State) {
        let files: Vec<path::PathBuf> = ctx.input_mut(|input| {
            std::mem::take(&mut input.raw.dropped_files)
                .into_iter()
                .filter_map(|file| file.path)
                .collect()
        });
        if files.is_empty() {
            return;
        }
        if state.phase == desktop::Phase::Demo {
            self.notice = Some("The demo has no remote host to upload to.".to_owned());
            self.notice_until = None;
            return;
        }
        if !matches!(
            state.phase,
            desktop::Phase::Watching | desktop::Phase::Resynchronizing
        ) {
            self.notice = Some("Connect before dropping files.".to_owned());
            self.notice_until = None;
            return;
        }
        if self.upload.is_some() {
            self.notice = Some("Already uploading.".to_owned());
            self.notice_until = None;
            return;
        }
        let Some(target) = self.focused.and_then(|pane| state.target(pane)) else {
            self.notice = Some("Click a live pane before dropping files.".to_owned());
            self.notice_until = None;
            return;
        };
        let options = match self.form.connection() {
            Ok(connection) => connection.options,
            Err(error) => {
                self.notice = Some(error.to_string());
                self.notice_until = None;
                return;
            }
        };
        let mut sized = Vec::new();
        for file in &files {
            let meta = match fs::metadata(file) {
                Ok(meta) if meta.is_file() => meta,
                _ => {
                    self.notice = Some(format!(
                        "{} is not a regular file; drop files only",
                        file.display()
                    ));
                    self.notice_until = None;
                    return;
                }
            };
            let name =
                crate::sftp::remote_file_name(file).unwrap_or_else(|_| file.display().to_string());
            sized.push((name, meta.len()));
        }
        if let Some(label) = crate::sftp::oversize_notice(&sized) {
            self.drop_prompt = Some(PendingDrop {
                files,
                label,
                target,
                options,
                allow_oversize: true,
            });
            self.notice = None;
            self.notice_until = None;
            return;
        }
        self.start_upload(
            state,
            PendingDrop {
                files,
                label: String::new(),
                target,
                options,
                allow_oversize: false,
            },
        );
    }

    fn start_upload(&mut self, state: &desktop::State, pending: PendingDrop) {
        if state.target(pending.target.pane()) != Some(pending.target) {
            self.notice = Some("The pane changed; drop the files again.".to_owned());
            self.notice_until = None;
            return;
        }
        let PendingDrop {
            files,
            target,
            options,
            allow_oversize,
            ..
        } = pending;
        let max_bytes = if allow_oversize {
            None
        } else {
            Some(crate::sftp::MAX_FILE_BYTES)
        };
        let (tx, rx) = sync::mpsc::sync_channel(4);
        let cancel = sync::Arc::new(sync::atomic::AtomicBool::new(false));
        let worker_cancel = sync::Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("starcom-sftp".to_owned())
            .spawn(move || {
                let progress = tx.clone();
                let result = crate::sftp::put_files_while(
                    &options,
                    &files,
                    || !worker_cancel.load(sync::atomic::Ordering::Acquire),
                    max_bytes,
                    |update| {
                        let _ = progress.try_send(UploadEvent::Progress {
                            name: update.name.to_owned(),
                            done: update.done,
                            total: update.total,
                        });
                    },
                );
                let _ = tx.send(match result {
                    Ok(paths) => UploadEvent::Done(paths),
                    Err(error) => {
                        UploadEvent::Failed(format!("{error:#}").chars().take(1024).collect())
                    }
                });
            });
        match worker {
            Ok(_) => {
                navigato_support::feature(navigato_support::Feature::Upload);
                self.upload = Some(Upload {
                    events: rx,
                    target,
                    cancel,
                });
                self.upload_progress = Some(("…".to_owned(), 0, 0));
                self.notice = None;
                self.notice_until = None;
            }
            Err(error) => {
                self.notice = Some(format!("Could not start file upload: {error}"));
                self.notice_until = None;
            }
        }
    }

    fn poll_upload(&mut self, ctx: &egui::Context) -> Option<(desktop::Target, Vec<String>)> {
        let upload = self.upload.as_ref()?;
        let target = upload.target;
        let mut last_progress = None;
        loop {
            match upload.events.try_recv() {
                Ok(UploadEvent::Progress { name, done, total }) => {
                    last_progress = Some((name, done, total));
                }
                Ok(UploadEvent::Done(paths)) => {
                    self.upload = None;
                    self.upload_progress = None;
                    return Some((target, paths));
                }
                Ok(UploadEvent::Failed(error)) => {
                    self.upload = None;
                    self.upload_progress = None;
                    self.notice = Some(error);
                    self.notice_until = None;
                    return None;
                }
                Err(sync::mpsc::TryRecvError::Empty) => break,
                Err(sync::mpsc::TryRecvError::Disconnected) => {
                    self.upload = None;
                    self.upload_progress = None;
                    self.notice = Some("Upload ended without a result.".to_owned());
                    self.notice_until = None;
                    return None;
                }
            }
        }
        if let Some(progress) = last_progress {
            self.upload_progress = Some(progress);
        }
        ctx.request_repaint_after(time::Duration::from_millis(50));
        None
    }

    /// `whole` is the status-bar Copy button: always the visible pane.
    /// Keyboard copy still prefers a live selection.
    fn copy_pane(&mut self, ctx: &egui::Context, state: &mut desktop::State, whole: bool) {
        let Some(id) = self.focused else {
            self.notice = Some("Click a pane first.".to_owned());
            self.notice_until = None;
            return;
        };
        let Some(pane) = state
            .view
            .as_mut()
            .and_then(|view| view.panes_mut().get_mut(&id))
        else {
            self.notice = Some("Click a pane first.".to_owned());
            self.notice_until = None;
            return;
        };
        let text = if !whole {
            pane.terminal.selected_text()
        } else {
            None
        }
        .unwrap_or_else(|| pane.terminal.screen_lines().join("\n"));
        terminal::copy(
            ctx,
            text,
            &mut self.notice,
            &mut self.notice_until,
            "Copied!",
        );
        pane.terminal.clear_selection();
    }

    #[cfg(test)]
    pub fn smoke_selection(&self, ctx: &egui::Context) -> (egui::Pos2, egui::Pos2) {
        let rect = self
            .pane_ui
            .values()
            .next()
            .map(|pane| pane.rect)
            .unwrap_or(egui::Rect::from_min_size(
                egui::pos2(20.0, 80.0),
                egui::vec2(500.0, 400.0),
            ));
        let font = egui::FontId::monospace(self.font_size);
        let width = ctx.fonts_mut(|fonts| fonts.glyph_width(&font, 'M'));
        let start = rect.min + egui::vec2(8.0 + width * 0.2, 48.0);
        let end = start + egui::vec2(width * 6.8, 0.0);
        (start, end)
    }
}

fn upload_label(name: &str, done: u64, total: u64) -> String {
    let short = if name.len() > 24 {
        format!("…{}", &name[name.len() - 23..])
    } else {
        name.to_owned()
    };
    if total >= 1024 * 1024 {
        format!(
            "{short}  {:.1}/{:.1} MiB",
            done as f32 / (1024.0 * 1024.0),
            total as f32 / (1024.0 * 1024.0)
        )
    } else {
        format!(
            "{short}  {:.0}/{:.0} KiB",
            done as f32 / 1024.0,
            total as f32 / 1024.0
        )
    }
}

fn paste_remote_paths(paths: &[String]) -> anyhow::Result<String> {
    let quoted: anyhow::Result<Vec<_>> = paths
        .iter()
        .map(|path| crate::command::shell_quote(path))
        .collect();
    quoted.map(|paths| paths.join(" "))
}

/// Click-only: arrows, Tab, and Escape must stay with the focused pane.
fn click_button(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) -> egui::Response {
    ui.add(egui::Button::new(text).sense(egui::Sense::CLICK))
}

/// A fixed-size three-dot activity mark. Only opacity changes, so tab and
/// button labels never pulse in width or shape.
pub(crate) fn paint_activity_indicator(ui: &egui::Ui, rect: egui::Rect, time: f64) {
    let step = (time * 5.0).floor() as u64;
    paint_activity_dots(ui, rect, step);
}

/// A refresh mark driven by visible changes rather than a timer. Its bright
/// dot advances with content/scroll activity and stays still when idle.
fn paint_refresh_indicator(ui: &egui::Ui, rect: egui::Rect, tick: u64) {
    paint_activity_dots(ui, rect, tick);
}

fn paint_activity_dots(ui: &egui::Ui, rect: egui::Rect, step: u64) {
    let center = rect.center();
    let color = ui.visuals().strong_text_color();
    let spacing = rect.width().min(rect.height()) * 0.31;
    for index in 0..3 {
        let strength = if index as u64 == step % 3 { 1.0 } else { 0.28 };
        let dot = center + egui::vec2((index as f32 - 1.0) * spacing, 0.0);
        ui.painter()
            .circle_filled(dot, 1.7, color.gamma_multiply(strength));
    }
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .char_limit(4096),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `~/.ssh/<file>` after `expand_path`, including Windows separators.
    fn test_config(text: &str) -> ssh_config::Config {
        ssh_config::Config::parse(text, path::Path::new("/home/test")).unwrap()
    }

    fn tilde_identity(file: &str) -> String {
        path::PathBuf::from("/home/test")
            .join(format!(".ssh/{file}"))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_literal_alias_populates_the_connection_form() {
        let config = sync::Arc::new(test_config(
            "Host dev\nHostName 10.0.0.2\nUser alice\nPort 2222\nIdentityFile ~/.ssh/dev",
        ));
        let mut ui = DesktopUi::with_config(config, None);
        ui.form.destination = "dev".to_owned();
        ui.refresh_profile();
        assert_eq!(ui.form.host, "10.0.0.2");
        assert_eq!(ui.form.user, "alice");
        assert_eq!(ui.form.port, 2222);
        assert_eq!(ui.form.identity_files, [tilde_identity("dev")]);
        assert!(!ui.form.identities_only);
        assert!(ui.form.host_key_alias.is_none());
    }

    #[test]
    fn hostkeyalias_is_carried_onto_the_connection() {
        let mut form = Form {
            user: "alice".into(),
            known_hosts: "/tmp/known_hosts".into(),
            ..Form::default()
        };
        form.apply_profile(
            ssh_config::Profile {
                host: "10.2.3.4".into(),
                host_key_alias: Some("trusted.example".into()),
                ..ssh_config::Profile::default()
            },
            Vec::new(),
        );
        let connection = form.connection_named("work").unwrap();
        assert_eq!(connection.options.host, "10.2.3.4");
        assert_eq!(
            connection.options.host_key_alias.as_deref(),
            Some("trusted.example")
        );
    }

    #[test]
    fn a_host_without_user_uses_the_local_account() {
        let mut form = Form {
            user: "previous".into(),
            ..Form::default()
        };
        form.apply_profile(
            ssh_config::Profile {
                host: "zork.example".into(),
                ..ssh_config::Profile::default()
            },
            Vec::new(),
        );
        assert_eq!(form.user, desktop::local_user());
    }

    #[test]
    fn a_host_without_identityfile_uses_every_default_key() {
        let mut form = Form::default();
        form.apply_profile(
            ssh_config::Profile {
                host: "zork.example".into(),
                ..ssh_config::Profile::default()
            },
            vec![
                path::PathBuf::from("/home/test/.ssh/id_ed25519"),
                path::PathBuf::from("/home/test/.ssh/id_rsa"),
            ],
        );
        assert_eq!(form.host, "zork.example");
        assert_eq!(
            form.identity_files,
            ["/home/test/.ssh/id_ed25519", "/home/test/.ssh/id_rsa"]
        );
        assert!(!form.identities_only);
    }

    #[test]
    fn identities_only_restricts_the_agent_to_configured_keys() {
        let mut form = Form::default();
        form.apply_profile(
            ssh_config::Profile {
                host: "zork.example".into(),
                identities_only: true,
                ..ssh_config::Profile::default()
            },
            vec![path::PathBuf::from("/home/test/.ssh/id_ed25519")],
        );
        assert_eq!(form.identity_files, ["/home/test/.ssh/id_ed25519"]);
        assert!(form.identities_only);
        form.user = "alice".into();
        form.known_hosts = "/tmp/known_hosts".into();
        let connection = form.connection_named("work").unwrap();
        assert!(connection.options.authentication.agent);
        assert!(connection.options.authentication.identities_only);
        assert_eq!(
            connection.options.authentication.files,
            [path::PathBuf::from("/home/test/.ssh/id_ed25519")]
        );
    }

    #[test]
    fn extra_identity_is_tried_before_config_files_then_the_agent() {
        let form = Form {
            host: "zork.example".into(),
            user: "alice".into(),
            known_hosts: "/tmp/known_hosts".into(),
            identity: "/home/alice/.ssh/extra".into(),
            identity_files: vec![
                "/home/alice/.ssh/work".into(),
                "/home/alice/.ssh/extra".into(),
            ],
            ..Form::default()
        };
        let connection = form.connection_named("work").unwrap();
        assert_eq!(
            connection.options.authentication.files,
            [
                path::PathBuf::from("/home/alice/.ssh/extra"),
                path::PathBuf::from("/home/alice/.ssh/work"),
            ]
        );
        assert!(connection.options.authentication.agent);
    }

    #[test]
    fn restored_tabs_re_read_identity_policy_from_config() {
        let config = sync::Arc::new(test_config(
            "Host zork\nHostName zork.example\nIdentityFile ~/.ssh/work\nIdentityFile ~/.ssh/id_ed25519\nIdentitiesOnly yes\nProxyCommand custom-proxy\n",
        ));
        let mut ui = DesktopUi::with_config(config, None);
        ui.restore(store::Tab {
            destination: "zork".into(),
            host: "saved.example".into(),
            user: "alice".into(),
            session: "work".into(),
            port: 22,
            identity: "/home/alice/.ssh/extra".into(),
            known_hosts: "/tmp/known_hosts".into(),
            ..store::Tab::default()
        });
        assert_eq!(ui.form.host, "saved.example");
        assert_eq!(ui.form.identity, "/home/alice/.ssh/extra");
        assert_eq!(
            ui.form.identity_files,
            [tilde_identity("work"), tilde_identity("id_ed25519")]
        );
        assert!(ui.form.identities_only);
        assert!(
            ui.form
                .unsupported
                .iter()
                .any(|item| item == "proxycommand")
        );
        assert!(ui.form.connection_named("work").is_err());
    }

    #[test]
    fn new_ui_starts_on_connection_form() {
        let ui = DesktopUi::default();
        assert_eq!(ui.screen, Screen::Connection);
    }

    #[test]
    fn refresh_marker_tracks_visible_changes_and_scrolling_not_time() {
        let mut ui = DesktopUi::default();
        let mut state = desktop::State::interactive_demo().unwrap();
        ui.note_refresh(&state, false);
        let initial = ui.refresh_tick;

        ui.note_refresh(&state, false);
        assert_eq!(ui.refresh_tick, initial, "an idle frame is not activity");

        let pane = state
            .view
            .as_ref()
            .and_then(|view| view.panes().keys().next().copied())
            .unwrap();
        state
            .view
            .as_mut()
            .unwrap()
            .apply(tmuxctl::Notification::Output {
                pane,
                bytes: b"changed".to_vec(),
            });
        ui.note_refresh(&state, false);
        assert_eq!(ui.refresh_tick, initial + 1);

        ui.note_refresh(&state, true);
        assert_eq!(ui.refresh_tick, initial + 2);
    }

    #[test]
    fn restored_pane_survives_connecting_and_is_selected_from_the_snapshot() {
        let mut ui = DesktopUi::default();
        ui.restore(store::Tab {
            window: Some(0),
            pane: Some(1),
            ..store::Tab::default()
        });
        ui.open_terminal();

        // Startup paints while SSH is still connecting. With no view to
        // validate against yet, the saved hint must not be discarded.
        ui.rebuild_layout(&desktop::State::default());
        assert_eq!(ui.saved().window, Some(0));
        assert_eq!(ui.saved().pane, Some(1));
        assert_eq!(ui.focused, None);

        let mut connected = desktop::State::interactive_demo().unwrap();
        connected.generation = 1;
        ui.rebuild_layout(&connected);
        assert_eq!(ui.window, Some(tmuxctl::WindowId(0)));
        assert_eq!(ui.selected, Some(tmuxctl::PaneId(1)));
        assert_eq!(ui.focused, Some(tmuxctl::PaneId(1)));
    }

    #[test]
    fn missing_restored_pane_falls_back_to_a_live_pane() {
        let mut ui = DesktopUi::default();
        ui.restore(store::Tab {
            window: Some(0),
            pane: Some(999),
            ..store::Tab::default()
        });
        ui.open_terminal();

        let mut connected = desktop::State::interactive_demo().unwrap();
        connected.generation = 1;
        ui.rebuild_layout(&connected);
        let selected = ui.selected.expect("a live fallback pane");
        assert!(
            connected
                .view
                .as_ref()
                .unwrap()
                .panes()
                .contains_key(&selected)
        );
        assert_eq!(ui.focused, Some(selected));
        assert_ne!(selected, tmuxctl::PaneId(999));
    }

    #[test]
    fn restored_pane_is_followed_when_it_moved_to_another_window() {
        let mut ui = DesktopUi::default();
        ui.restore(store::Tab {
            window: Some(0),
            pane: Some(1),
            ..store::Tab::default()
        });
        ui.open_terminal();

        let mut connected = desktop::State::interactive_demo().unwrap();
        connected.generation = 1;
        let moved = connected
            .view
            .as_mut()
            .unwrap()
            .panes_mut()
            .get_mut(&tmuxctl::PaneId(1))
            .unwrap();
        moved.state.window = tmuxctl::WindowId(7);
        moved.state.left = 0;
        ui.rebuild_layout(&connected);

        assert_eq!(ui.window, Some(tmuxctl::WindowId(7)));
        assert_eq!(ui.selected, Some(tmuxctl::PaneId(1)));
    }

    #[test]
    fn a_layout_rebuild_keeps_a_scrolled_history_viewport() {
        let mut ui = DesktopUi::default();
        ui.open_terminal();
        let mut connected = desktop::State::interactive_demo().unwrap();
        connected.generation = 1;
        let pane = tmuxctl::PaneId(1);
        {
            let terminal = &mut connected
                .view
                .as_mut()
                .unwrap()
                .panes_mut()
                .get_mut(&pane)
                .unwrap()
                .terminal;
            terminal.scroll_history(10);
            assert!(terminal.history_offset() > 0);
        }
        ui.rebuild_layout(&connected);
        let offset = connected
            .view
            .as_ref()
            .unwrap()
            .panes()
            .get(&pane)
            .unwrap()
            .terminal
            .history_offset();
        ui.pane_ui
            .entry(pane)
            .or_default()
            .keep_history_viewport(offset);

        let mut next = desktop::State::interactive_demo().unwrap();
        next.generation = 2;
        next.view
            .as_mut()
            .unwrap()
            .preserve_history_offsets(connected.view.as_ref().unwrap());
        ui.rebuild_layout(&next);
        assert!(
            !ui.pane_ui.get(&pane).expect("kept pane ui").is_stuck(),
            "a scrolled pane must not snap back to the live tip after zoom"
        );
        assert_eq!(
            next.view
                .as_ref()
                .unwrap()
                .panes()
                .get(&pane)
                .unwrap()
                .terminal
                .history_offset(),
            offset
        );
    }

    #[test]
    fn a_layout_rebuild_keeps_live_tip_tracking() {
        let mut ui = DesktopUi::default();
        ui.open_terminal();
        let mut connected = desktop::State::interactive_demo().unwrap();
        connected.generation = 1;
        let pane = tmuxctl::PaneId(1);
        ui.rebuild_layout(&connected);
        ui.pane_ui.entry(pane).or_default();
        assert!(ui.pane_ui.get(&pane).unwrap().is_stuck());

        connected
            .view
            .as_mut()
            .unwrap()
            .panes_mut()
            .get_mut(&pane)
            .unwrap()
            .terminal
            .scroll_history(10);
        let mut next = desktop::State::interactive_demo().unwrap();
        next.generation = 2;
        next.view
            .as_mut()
            .unwrap()
            .preserve_history_offsets(connected.view.as_ref().unwrap());
        ui.rebuild_layout(&next);
        assert!(
            ui.pane_ui.get(&pane).expect("kept pane ui").is_stuck(),
            "a pane following the live tip must still do so after zoom"
        );
    }

    fn paint(ui: &mut DesktopUi, state: &mut desktop::State) -> Action {
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let mut action = Action::None;
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 720.0),
                )),
                ..Default::default()
            },
            |root| {
                action = ui.show(root, state);
            },
        );
        action
    }

    #[test]
    fn choosing_a_host_lists_its_sessions() {
        let config = sync::Arc::new(test_config("Host zork\nHostName 10.0.0.2\nUser alice\n"));
        let mut ui = DesktopUi::with_config(config, None);
        ui.form.destination = "zork".to_owned();
        ui.refresh_profile();
        ui.auto_list = true;
        let mut state = desktop::State::default();
        assert!(matches!(
            paint(&mut ui, &mut state),
            Action::ListSessions(_)
        ));
        assert_eq!(ui.listed_destination, "zork");
    }

    #[test]
    fn a_session_list_selects_the_first_name() {
        let mut ui = DesktopUi::default();
        ui.form.destination = "zork".to_owned();
        ui.listed_destination = "zork".to_owned();
        let mut state = desktop::State::default();
        state.discovery = Some(desktop::Discovery::Sessions(vec![
            crate::sessions::Summary {
                name: "0".into(),
                windows: 1,
                attached: 0,
            },
            crate::sessions::Summary {
                name: "work".into(),
                windows: 2,
                attached: 1,
            },
        ]));
        paint(&mut ui, &mut state);
        assert_eq!(ui.form.session, "0");
    }

    #[test]
    fn a_listed_last_used_session_stays_selected() {
        let mut ui = DesktopUi::default();
        ui.form.destination = "zork".to_owned();
        ui.form.session = "work".to_owned();
        ui.listed_destination = "zork".to_owned();
        let mut state = desktop::State::default();
        state.discovery = Some(desktop::Discovery::Sessions(vec![
            crate::sessions::Summary {
                name: "0".into(),
                windows: 1,
                attached: 0,
            },
            crate::sessions::Summary {
                name: "work".into(),
                windows: 2,
                attached: 1,
            },
        ]));
        paint(&mut ui, &mut state);
        assert_eq!(ui.form.session, "work");
    }

    #[test]
    fn creating_a_session_connects_once_the_host_confirms() {
        let mut ui = DesktopUi::default();
        ui.form.destination = "zork".to_owned();
        ui.form.host = "10.0.0.2".to_owned();
        ui.listed_destination = "zork".to_owned();
        ui.creating = Some("fresh".to_owned());
        ui.form.session = "fresh".to_owned();
        let mut state = desktop::State::default();
        state.discovery = Some(desktop::Discovery::Created("fresh".into()));
        assert!(matches!(paint(&mut ui, &mut state), Action::Connect(_)));
        assert!(ui.creating.is_none());
        assert_eq!(ui.form.session, "fresh");
    }

    #[test]
    fn a_typed_new_session_name_is_not_replaced_by_the_listing() {
        let mut ui = DesktopUi::default();
        ui.form.destination = "zork".to_owned();
        ui.form.host = "10.0.0.2".to_owned();
        ui.listed_destination = "zork".to_owned();
        ui.create_name = "fresh".to_owned();
        let mut state = desktop::State::default();
        state.discovery = Some(desktop::Discovery::Sessions(vec![
            crate::sessions::Summary {
                name: "0".into(),
                windows: 1,
                attached: 0,
            },
        ]));
        paint(&mut ui, &mut state);
        assert_eq!(ui.create_name, "fresh");
        assert_eq!(ui.form.session, "0");
    }

    #[test]
    fn restored_tabs_do_not_list_on_first_paint() {
        let mut ui = DesktopUi::default();
        ui.restore(store::Tab {
            destination: "zork".into(),
            host: "zork.example".into(),
            user: "alice".into(),
            session: "work".into(),
            port: 22,
            ..store::Tab::default()
        });
        assert!(!ui.auto_list);
        assert_eq!(ui.listed_destination, "zork");
        let mut state = desktop::State::default();
        assert!(matches!(paint(&mut ui, &mut state), Action::None));
    }

    #[test]
    fn a_dropped_connection_keeps_the_last_view_on_screen() {
        let mut ui = DesktopUi::default();
        let mut state = desktop::State::interactive_demo().unwrap();
        paint(&mut ui, &mut state);
        assert_eq!(ui.screen, Screen::Terminal);
        let painted: usize = ui.pane_ui.values().map(|pane| pane.painted_rows).sum();
        assert!(painted > 0, "demo panes should paint");
        state.phase = desktop::Phase::Reconnecting;
        state.error =
            Some("The connection dropped. SSH Transport: No route to host (os error 113)".into());
        if let Some(ref mut view) = state.view {
            view.disconnect();
        }
        paint(&mut ui, &mut state);
        assert_eq!(ui.screen, Screen::Terminal);
        let painted: usize = ui.pane_ui.values().map(|pane| pane.painted_rows).sum();
        assert!(
            painted > 0,
            "the last view must stay readable while reconnecting"
        );
        let text = state
            .view
            .as_ref()
            .expect("last view")
            .panes()
            .values()
            .next()
            .expect("pane")
            .terminal
            .screen_lines()
            .join("\n");
        assert!(
            text.contains("Starcom") || text.contains("demo"),
            "{text:?}"
        );
    }

    #[test]
    fn a_failed_first_attach_returns_to_the_form() {
        let mut ui = DesktopUi::default();
        ui.open_terminal();
        let mut state = desktop::State::default();
        state.phase = desktop::Phase::Failed;
        state.error = Some(
            "Authentication failed. Starcom will not retry it. SSH Authentication: no SSH agent is available"
                .into(),
        );
        paint(&mut ui, &mut state);
        assert_eq!(ui.screen, Screen::Connection);
    }

    #[test]
    fn a_destroyed_session_disconnects_like_exit() {
        let mut ui = DesktopUi::default();
        let mut state = desktop::State::interactive_demo().unwrap();
        paint(&mut ui, &mut state);
        assert_eq!(ui.screen, Screen::Terminal);
        state.phase = desktop::Phase::Failed;
        state.failure = Some(reconnect::Failure::MissingSession);
        assert!(matches!(paint(&mut ui, &mut state), Action::Disconnect));
        let mut ui = DesktopUi::default();
        let mut state = desktop::State::interactive_demo().unwrap();
        paint(&mut ui, &mut state);
        state.phase = desktop::Phase::Failed;
        state.failure = Some(reconnect::Failure::ServerExit);
        assert!(
            matches!(paint(&mut ui, &mut state), Action::Disconnect),
            "a tmux server/session end is not a reconnect"
        );
    }

    #[test]
    fn a_published_view_replaces_the_form_once() {
        let mut ui = DesktopUi::default();
        let mut state = desktop::State::interactive_demo().unwrap();
        paint(&mut ui, &mut state);
        assert_eq!(ui.screen, Screen::Terminal);
        ui.screen = Screen::Connection;
        paint(&mut ui, &mut state);
        assert_eq!(
            ui.screen,
            Screen::Connection,
            "Connection settings must not be yanked back to the workspace"
        );
    }

    fn screen_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 720.0),
            )),
            ..Default::default()
        }
    }

    /// Lay the demo pane out and hold keyboard focus long enough for egui's
    /// last-frame EventFilter to lock arrows/tab/escape onto it.
    fn focus_demo_pane() -> (
        egui::Context,
        DesktopUi,
        desktop::State,
        tmuxctl::PaneId,
        desktop::Target,
    ) {
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let mut state = desktop::State::interactive_demo().unwrap();
        let pane = tmuxctl::PaneId(0);
        let target = state.target(pane).expect("interactive demo target");
        let mut ui = DesktopUi::default();
        ui.open_terminal();
        // rebuild_layout clears the focused pane on its first pass, so ask
        // again each frame. request_focus resets the event filter, so one
        // extra paint is needed after the last request.
        for _ in 0..4 {
            let _ = ctx.run_ui(screen_input(), |root| {
                ui.show(root, &mut state);
            });
            ui.focused = Some(pane);
            ctx.memory_mut(|memory| {
                memory.request_focus(terminal::focus_id(ui.generation, pane));
            });
        }
        let _ = ctx.run_ui(screen_input(), |root| {
            ui.show(root, &mut state);
        });
        // The client-size debounce is 200ms. On a slow runner that fires on
        // the next frame and would share a Frame with ArrowUp/paste.
        if let Some((size, _)) = ui.pending_client_cells.take() {
            ui.client_cells = Some(size);
        }
        assert!(
            ui.terminal_focused(&ctx),
            "the pane must hold focus or this test proves nothing"
        );
        (ctx, ui, state, pane, target)
    }

    /// A frame that produces a paste request AND keystrokes must deliver both,
    /// in order. Returning one action per frame used to discard the keystrokes.
    #[test]
    fn a_paste_request_does_not_displace_the_same_frame_keystrokes() {
        let (ctx, mut ui, mut state, _pane, target) = focus_demo_pane();
        let input = egui::RawInput {
            events: vec![
                egui::Event::Key {
                    key: egui::Key::Insert,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::SHIFT,
                },
                egui::Event::Text("ls".to_owned()),
            ],
            modifiers: egui::Modifiers::SHIFT,
            ..screen_input()
        };
        let mut action = Action::None;
        let _ = ctx.run_ui(input, |root| {
            action = ui.show(root, &mut state);
        });
        let Action::Frame(steps) = action else {
            panic!("expected an ordered terminal frame")
        };
        assert!(
            matches!(steps.as_slice(), [Step::RequestPaste(a), Step::Send(b, _)]
                if *a == target && *b == target),
            "the paste request and the keystrokes must both survive, in order"
        );
        assert!(ui.notice.is_none(), "nothing should have been dropped");
    }

    #[test]
    fn arrow_up_stays_with_the_focused_pane() {
        let (ctx, mut ui, mut state, pane, target) = focus_demo_pane();
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::ArrowUp,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..screen_input()
        };
        let mut action = Action::None;
        let _ = ctx.run_ui(input, |root| {
            action = ui.show(root, &mut state);
        });
        let Action::Frame(steps) = action else {
            panic!("expected the arrow to reach the pane");
        };
        // A slow machine can emit ClientSize on the same frame (200ms debounce).
        // That is not focus walking; the arrow still has to be in this frame.
        assert!(
            steps.iter().any(|step| matches!(
                step,
                Step::Send(
                    sent,
                    terminal_input::Action::Key(terminal_input::Key::Up, modifiers)
                ) if *sent == target && !modifiers.control && !modifiers.alt && !modifiers.shift
            )),
            "ArrowUp must go to the remote application, not another widget ({} steps)",
            steps.len()
        );
        assert_eq!(ui.focused, Some(pane));
        assert!(
            ui.terminal_focused(&ctx),
            "egui must not walk focus to Paste or any other widget"
        );
    }

    #[test]
    fn losing_keyboard_focus_keeps_the_last_pane_for_restore() {
        let (ctx, mut ui, mut state, pane, _target) = focus_demo_pane();
        ctx.memory_mut(|memory| {
            memory.surrender_focus(terminal::focus_id(ui.generation, pane));
        });
        assert!(
            !ui.terminal_focused(&ctx),
            "surrender must drop logical focus before the next paint"
        );
        let _ = ctx.run_ui(screen_input(), |root| {
            ui.show(root, &mut state);
        });
        assert!(
            ui.focused.is_none(),
            "the white border must not outlive keyboard focus"
        );
        assert_eq!(ui.selected, Some(pane));
        assert_eq!(ui.saved().pane, Some(pane.0));
    }

    #[test]
    fn jump_routes_are_shared_by_listing_connect_and_restore() {
        let config = sync::Arc::new(test_config(
            "Host target\nHostName target.example\nUser alice\nProxyJump relay\n\
             Host relay\nHostName relay.example\nUser bob\nPort 2222\nIdentityFile ~/.ssh/relay\n",
        ));
        let mut ui = DesktopUi::with_config(config, None);
        ui.form.destination = "target".into();
        ui.form.session = "work".into();
        ui.refresh_profile();
        let listed = ui.form.listing().unwrap();
        let attached = ui.form.connection().unwrap();
        assert_eq!(listed.options.jumps.len(), 1);
        assert_eq!(attached.options.jumps[0].host, "relay.example");
        assert_eq!(attached.options.jumps[0].user, "bob");
        assert_eq!(attached.options.jumps[0].port, 2222);
        assert_eq!(
            attached.options.jumps[0].authentication.files,
            [path::PathBuf::from("/home/test/.ssh/relay")]
        );
        let saved = ui.form.saved();
        ui.restore(saved);
        assert_eq!(ui.resume().unwrap().options.jumps[0].host, "relay.example");
    }

    #[test]
    fn unsupported_bastion_blocks_restored_connections() {
        let config = sync::Arc::new(test_config(
            "Host target\nProxyJump relay\nHost relay\nProxyCommand custom-proxy\n",
        ));
        let mut ui = DesktopUi::with_config(config, None);
        ui.restore(store::Tab {
            destination: "target".into(),
            host: "reachable.example".into(),
            user: "alice".into(),
            session: "work".into(),
            port: 22,
            known_hosts: "/tmp/known_hosts".into(),
            ..store::Tab::default()
        });
        assert!(
            ui.form
                .profile_error
                .as_deref()
                .unwrap()
                .contains("proxycommand")
        );
        assert!(ui.resume().is_err());
    }
}
