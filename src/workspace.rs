//! Connection tabs own independent clients, forms, selection and input tokens.
//! A new tab displays a form, never a sidebar alongside somebody else's panes.

use std::{fs, io, path, sync, time};

use anyhow::Context;

use crate::{core, desktop, dialog, reconnect, ssh_config, store, ui};

const MAX_TABS: usize = 16;
const NEW_CONNECTION: &str = "New connection";
type Wake = sync::Arc<dyn Fn() + Send + Sync>;

struct SessionRename {
    id: u64,
    draft: String,
    focus: bool,
}

struct Tab {
    id: u64,
    label: String,
    client: desktop::Client,
    ui: ui::DesktopUi,
    last_seq: u64,
    last_output: time::Instant,
    last_phase: desktop::Phase,
    last_revision: u64,
}

pub(crate) enum Action {
    None,
    New,
    Select(u64),
    Close(u64),
    /// Move `id` so it occupies `insert_at` in the current tab list.
    Reorder {
        id: u64,
        insert_at: usize,
    },
    Tab(u64, Box<ui::Action>),
}

pub(crate) struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    /// The "+" form. Not a registered tab until Connect succeeds.
    composer: Tab,
    composer_open: bool,
    next: u64,
    wake: Wake,
    config: sync::Arc<ssh_config::Config>,
    config_error: Option<String>,
    notice: Option<String>,
    /// Where saved tabs live. None disables persistence entirely, which is what
    /// happens with no home directory and in the demo.
    store: Option<path::PathBuf>,
    fps: u32,
    idle: u32,
    restore_tabs: bool,
    about: bool,
    about_icon: Option<egui::TextureHandle>,
    /// Seconds this install has been open across launches. Updated on persist.
    open_secs: u64,
    session_started: time::Instant,
    /// Remote frames run this fast after keys or wheel, so echo is not stuck
    /// on the idle fps cap. None when idle.
    echo_until: Option<time::Instant>,
    /// GUI-side copy of the last event-loop clock, used to notice a machine
    /// sleep while the SSH worker is blocked in poll.
    suspend_clock: reconnect::AliveClock,
    /// A local action was applied after the frame that produced it, so one more
    /// paint is required even if no client worker changed.
    local_dirty: bool,
    /// The winit lifecycle can ask us to shut down through more than one path.
    /// Only the first call may persist and clear the tab list.
    shut_down: bool,
    /// In-place session rename on a tab chip.
    renaming: Option<SessionRename>,
}

/// A restored tab is labelled by where it points, not by a live connection.
fn label(tab: &store::Tab) -> String {
    let destination = if tab.destination.trim().is_empty() {
        tab.host.trim()
    } else {
        tab.destination.trim()
    };
    match (destination.is_empty(), tab.session.trim()) {
        (true, "") => NEW_CONNECTION.to_owned(),
        (true, session) => session.to_owned(),
        (false, "") => destination.to_owned(),
        (false, session) => format!("{destination} / {session}"),
    }
}

fn drop_insert_at(
    response: &egui::Response,
    tab_id: u64,
    index: usize,
    pointer: Option<egui::Pos2>,
) -> Option<usize> {
    let dragged = response.dnd_hover_payload::<u64>()?;
    if *dragged == tab_id {
        return None;
    }
    let pointer = pointer?;
    Some(if pointer.x < response.rect.center().x {
        index
    } else {
        index + 1
    })
}

fn format_open(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    match (days, hours, minutes, secs) {
        (0, 0, 0, s) => format!("{s}s"),
        (0, 0, m, _) => format!("{m}m"),
        (0, h, m, _) => format!("{h}h {m}m"),
        (d, h, _, _) => format!("{d}d {h}h"),
    }
}

fn about_icon() -> Option<egui::ColorImage> {
    const PNG: &[u8] = include_bytes!("../etc/macos/icon_128.png");
    let mut reader = png::Decoder::new(io::Cursor::new(PNG)).read_info().ok()?;
    let mut pixels = vec![0_u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    pixels.truncate(info.buffer_size());
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [info.width as usize, info.height as usize],
        &pixels,
    ))
}

fn ack_painted(tab: &mut Tab) {
    let state = tab.client.lock();
    tab.last_revision = state.revision();
}

fn spawn_tab(
    id: u64,
    wake: Wake,
    config: sync::Arc<ssh_config::Config>,
    config_load_error: Option<String>,
) -> anyhow::Result<Tab> {
    Ok(Tab {
        id,
        label: NEW_CONNECTION.into(),
        client: desktop::Client::new(wake)?,
        ui: ui::DesktopUi::with_config(config, config_load_error),
        last_seq: 0,
        last_output: time::Instant::now(),
        last_phase: desktop::Phase::Idle,
        last_revision: 0,
    })
}

fn busy_phase(phase: desktop::Phase) -> bool {
    matches!(
        phase,
        desktop::Phase::Connecting | desktop::Phase::Reconnecting
    )
}

fn live_phase(phase: desktop::Phase) -> bool {
    matches!(
        phase,
        desktop::Phase::Watching | desktop::Phase::Demo | desktop::Phase::Resynchronizing
    )
}

fn lift(color: egui::Color32, by: u8) -> egui::Color32 {
    egui::Color32::from_rgb(
        color.r().saturating_add(by),
        color.g().saturating_add(by),
        color.b().saturating_add(by),
    )
}

fn tab_color(phase: desktop::Phase, idle: egui::Color32, quiet: bool) -> egui::Color32 {
    match phase {
        desktop::Phase::Watching | desktop::Phase::Demo | desktop::Phase::Resynchronizing
            if quiet =>
        {
            egui::Color32::from_rgb(36, 88, 148)
        }
        desktop::Phase::Watching | desktop::Phase::Demo | desktop::Phase::Resynchronizing => {
            egui::Color32::from_rgb(38, 98, 58)
        }
        desktop::Phase::Connecting | desktop::Phase::Reconnecting => {
            egui::Color32::from_rgb(140, 108, 28)
        }
        desktop::Phase::Failed => egui::Color32::from_rgb(140, 108, 28),
        _ => idle,
    }
}

fn paint_tab_fills(ui: &mut egui::Ui, fill: egui::Color32, hover: egui::Color32) {
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.inactive.weak_bg_fill = fill;
    widgets.inactive.bg_fill = fill;
    widgets.hovered.weak_bg_fill = hover;
    widgets.hovered.bg_fill = hover;
    widgets.active.weak_bg_fill = hover;
    widgets.active.bg_fill = hover;
    widgets.open.weak_bg_fill = fill;
    widgets.open.bg_fill = fill;
}

fn paint_drop_marker(ui: &egui::Ui, rect: egui::Rect, after: bool) {
    let x = if after {
        rect.right() + 2.0
    } else {
        rect.left() - 2.0
    };
    ui.painter().vline(
        x,
        rect.y_range(),
        egui::Stroke::new(3.0_f32, ui.visuals().selection.stroke.color),
    );
}

impl Workspace {
    pub fn new(wake: Wake, startup: desktop::Startup) -> anyhow::Result<Self> {
        Self::try_new(wake, startup, |_, _| dialog::BrokenStore::Exit)?
            .ok_or_else(|| anyhow::anyhow!("saved tabs were unreadable"))
    }

    /// Open a workspace, asking with a system dialog if saved tabs cannot be
    /// read. `None` means the user chose to exit before the window opened.
    pub(crate) fn launch(wake: Wake, startup: desktop::Startup) -> anyhow::Result<Option<Self>> {
        Self::try_new(wake, startup, dialog::ask_clear_or_exit)
    }

    fn try_new(
        wake: Wake,
        startup: desktop::Startup,
        on_broken: impl Fn(&path::Path, &anyhow::Error) -> dialog::BrokenStore,
    ) -> anyhow::Result<Option<Self>> {
        let wake_composer = sync::Arc::clone(&wake);
        let mut workspace = Self {
            tabs: Vec::new(),
            active: 0,
            composer: spawn_tab(
                1,
                wake_composer,
                sync::Arc::new(ssh_config::Config::default()),
                None,
            )?,
            composer_open: true,
            next: 2,
            wake,
            config: sync::Arc::new(ssh_config::Config::default()),
            config_error: None,
            notice: None,
            // The demo must not read or overwrite a real saved workspace.
            store: (startup != desktop::Startup::Demo)
                .then(desktop::home_path)
                .flatten()
                .map(|home| store::path(&home)),
            fps: store::DEFAULT_FPS,
            idle: store::DEFAULT_IDLE,
            restore_tabs: true,
            about: false,
            about_icon: None,
            open_secs: 0,
            session_started: time::Instant::now(),
            echo_until: None,
            suspend_clock: reconnect::AliveClock::now(),
            local_dirty: false,
            shut_down: false,
            renaming: None,
        };
        if startup != desktop::Startup::Demo {
            workspace.reload_config();
            if !workspace.restore(on_broken)? {
                return Ok(None);
            }
        }
        if startup == desktop::Startup::Demo {
            workspace.push_idle_tab()?;
            workspace.composer_open = false;
            workspace.tabs[0].client.demo()?;
            workspace.tabs[0].label = "Demo".into();
            workspace.tabs[0].ui.open_terminal();
        } else {
            workspace.composer_open = workspace.tabs.is_empty();
        }
        Ok(Some(workspace))
    }

    /// Reopen saved tabs and reconnect each complete saved host/session. A tab
    /// with invalid or incomplete saved settings stays on its connection form.
    /// `Ok(false)` means the user chose to exit and the file was left alone.
    fn restore(
        &mut self,
        on_broken: impl Fn(&path::Path, &anyhow::Error) -> dialog::BrokenStore,
    ) -> anyhow::Result<bool> {
        self.restore_with(on_broken, |client, connection| client.connect(connection))
    }

    fn restore_with(
        &mut self,
        on_broken: impl Fn(&path::Path, &anyhow::Error) -> dialog::BrokenStore,
        mut resume: impl FnMut(&desktop::Client, desktop::Connection) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        let Some(file) = self.store.clone() else {
            return Ok(true);
        };
        let saved = match store::load(&file) {
            Ok(Some(saved)) => saved,
            Ok(None) => return Ok(true),
            Err(error) => match on_broken(&file, &error) {
                dialog::BrokenStore::Exit => {
                    // Disable saving so a later persist cannot overwrite a file
                    // we refused to clear.
                    self.store = None;
                    return Ok(false);
                }
                dialog::BrokenStore::Clear => {
                    match fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error).context(format!("clear {}", file.display()));
                        }
                    }
                    return Ok(true);
                }
            },
        };
        self.restore_tabs = saved.restore_tabs;
        if self.restore_tabs {
            for tab in saved.tabs {
                if self.push_idle_tab().is_err() {
                    break;
                }
                let index = self.tabs.len() - 1;
                let restored = &mut self.tabs[index];
                restored.label = label(&tab);
                restored.ui.restore(tab);
                match restored.ui.resume() {
                    Ok(connection) => {
                        if let Err(error) = resume(&restored.client, connection) {
                            restored.ui.return_to_form();
                            restored.client.lock().error =
                                Some(format!("Could not resume this tab: {error}"));
                        }
                    }
                    Err(error) => {
                        restored.client.lock().error =
                            Some(format!("Could not resume this tab: {error}"));
                    }
                }
            }
        }
        self.active = saved.active.min(self.tabs.len().saturating_sub(1));
        self.composer_open = self.tabs.is_empty();
        self.fps = store::clamp_fps(saved.fps);
        self.idle = saved.idle.min(store::MAX_IDLE);
        self.open_secs = saved.open_secs.min(store::MAX_OPEN_SECS);
        Ok(true)
    }

    pub(crate) fn repaint_interval(&self) -> time::Duration {
        time::Duration::from_secs_f64(1.0 / f64::from(store::clamp_fps(self.fps)))
    }

    /// Idle remote paint uses `fps` from the saved workspace. After we sent
    /// input, echo is allowed up to 20 fps for a short window so typing is not
    /// 200ms behind.
    pub(crate) fn paint_interval(&self) -> time::Duration {
        let idle = self.repaint_interval();
        if self
            .echo_until
            .is_some_and(|until| time::Instant::now() < until)
        {
            idle.min(time::Duration::from_millis(50))
        } else {
            idle
        }
    }

    /// Report whether anything currently visible changed. Worker revisions for
    /// the selected terminal stay pending until `show` paints it; consuming
    /// them here made a later coalesced wake look like a duplicate and skip
    /// the real frame. Hidden-tab output still does not repaint the selected
    /// terminal.
    pub(crate) fn remote_changed(&mut self) -> bool {
        let mut repaint = self.local_dirty;
        let now = time::Instant::now();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let state = tab.client.lock();
            let revision = state.revision();
            if revision == tab.last_revision {
                continue;
            }
            let phase = state.phase;
            let seq = state
                .view
                .as_ref()
                .map(crate::snapshot::View::display_seq)
                .unwrap_or(0);
            drop(state);

            let phase_changed = phase != tab.last_phase;
            let display_changed = seq != tab.last_seq;
            let was_quiet = self.idle > 0
                && live_phase(tab.last_phase)
                && now.saturating_duration_since(tab.last_output)
                    >= time::Duration::from_secs(u64::from(self.idle));
            let visible = !self.composer_open && index == self.active;
            let chip = phase_changed || (display_changed && was_quiet);
            if visible || chip {
                repaint = true;
            }
            if !visible {
                tab.last_revision = revision;
                if phase_changed || display_changed {
                    tab.last_phase = phase;
                    tab.last_seq = seq;
                    tab.last_output = now;
                }
            }
        }

        let state = self.composer.client.lock();
        let revision = state.revision();
        if revision != self.composer.last_revision {
            if self.composer_open {
                repaint = true;
            } else {
                self.composer.last_revision = revision;
            }
        }
        drop(state);
        repaint
    }

    fn show_about(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(time::Duration::from_secs(1));
        let mut fps = self.fps;
        let mut idle = self.idle;
        let mut restore_tabs = self.restore_tabs;
        let mut close = false;
        let open_for = format_open(self.open_secs_now());
        if self.about_icon.is_none()
            && let Some(icon) = about_icon()
        {
            self.about_icon =
                Some(ctx.load_texture("starcom-about-icon", icon, egui::TextureOptions::LINEAR));
        }
        let icon = self.about_icon.clone();
        let id = egui::Id::new("starcom-about");
        // First-frame Area size is 0 unless we name one; that clips the
        // contents and flashes egui's debug overflow (red) edges.
        let response = egui::Modal::new(id)
            .area(egui::Modal::default_area(id).default_size([400.0, 340.0]))
            .show(ctx, |ui| {
                ui.set_min_size(egui::vec2(380.0, 300.0));
                ui.set_width(380.0);
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 100.0).max(180.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some(ref icon) = icon {
                                ui.add(egui::Image::new((icon.id(), egui::vec2(72.0, 72.0))));
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Starcom {}",
                                        env!("CARGO_PKG_VERSION")
                                    ))
                                    .heading(),
                                );
                                ui.hyperlink_to(
                                    "github.com/navigato-rs/starcom",
                                    "https://github.com/navigato-rs/starcom",
                                );
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    ui.label("by");
                                    ui.label(egui::RichText::new("Dzmitry Malyshau").italics());
                                    ui.label("aka");
                                    ui.hyperlink_to("@kvark", "https://github.com/kvark");
                                });
                            });
                        });
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.label("Idle paint rate");
                            ui.add(
                                egui::DragValue::new(&mut fps)
                                    .range(1..=store::MAX_FPS)
                                    .suffix(" fps"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Turn a quiet tab blue after");
                            ui.add(
                                egui::DragValue::new(&mut idle)
                                    .range(0..=store::MAX_IDLE)
                                    .suffix(" seconds"),
                            );
                        });
                        ui.weak("0 seconds keeps a connected tab green.");
                        ui.add_space(6.0);
                        ui.checkbox(&mut restore_tabs, "Resume open tabs on startup");
                        ui.weak("Reconnects each saved host and tmux session automatically.");
                        ui.add_space(10.0);
                        ui.label(format!("Open for {open_for} in total"));
                        navigato_support::show(ui, crate::SUPPORT);
                        ui.add_space(8.0);
                    });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        if close || response.should_close() {
            self.about = false;
        }
        if store::clamp_fps(fps) != self.fps
            || idle.min(store::MAX_IDLE) != self.idle
            || restore_tabs != self.restore_tabs
        {
            self.fps = store::clamp_fps(fps);
            self.idle = idle.min(store::MAX_IDLE);
            self.restore_tabs = restore_tabs;
            self.persist();
        }
    }

    fn fold_open_time(&mut self) {
        let elapsed = self.session_started.elapsed().as_secs();
        self.open_secs = self
            .open_secs
            .saturating_add(elapsed)
            .min(store::MAX_OPEN_SECS);
        self.session_started = time::Instant::now();
    }

    fn open_secs_now(&self) -> u64 {
        self.open_secs
            .saturating_add(self.session_started.elapsed().as_secs())
            .min(store::MAX_OPEN_SECS)
    }

    /// Persist after a change to which tabs exist or where they point. Failure
    /// is reported once and then disables saving, rather than repeating on
    /// every action.
    fn persist(&mut self) {
        let Some(file) = self.store.clone() else {
            return;
        };
        self.fold_open_time();
        let saved = store::Workspace {
            tabs: self
                .tabs
                .iter()
                .take(store::MAX_TABS)
                .map(|tab| tab.ui.saved())
                .collect(),
            active: self.active,
            restore_tabs: self.restore_tabs,
            fps: store::clamp_fps(self.fps),
            idle: self.idle.min(store::MAX_IDLE),
            open_secs: self.open_secs.min(store::MAX_OPEN_SECS),
        };
        if let Err(error) = store::save(&file, &saved) {
            self.notice = Some(format!("Could not save tabs: {error:#}"));
            self.store = None;
        }
    }

    fn reorder(&mut self, id: u64, insert_at: usize) {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let insert_at = insert_at.min(self.tabs.len());
        if from == insert_at || from + 1 == insert_at {
            return;
        }
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        let tab = self.tabs.remove(from);
        let insert_at = if insert_at > from {
            insert_at - 1
        } else {
            insert_at
        };
        self.tabs.insert(insert_at.min(self.tabs.len()), tab);
        if let Some(id) = active_id {
            self.active = self.tabs.iter().position(|tab| tab.id == id).unwrap_or(0);
        }
    }

    fn open_composer(&mut self) {
        self.cancel_transient();
        self.composer_open = true;
    }

    fn push_idle_tab(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tabs.len() < MAX_TABS,
            "at most {MAX_TABS} connection tabs may be open"
        );
        let tab = spawn_tab(
            self.alloc_id(),
            sync::Arc::clone(&self.wake),
            sync::Arc::clone(&self.config),
            self.config_error.clone(),
        )?;
        self.tabs.push(tab);
        self.composer_open = false;
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    fn promote_composer(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tabs.len() < MAX_TABS,
            "at most {MAX_TABS} connection tabs may be open"
        );
        let replacement = spawn_tab(
            self.alloc_id(),
            sync::Arc::clone(&self.wake),
            sync::Arc::clone(&self.config),
            self.config_error.clone(),
        )?;
        let tab = std::mem::replace(&mut self.composer, replacement);
        self.tabs.push(tab);
        self.composer_open = false;
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.checked_add(1).expect("tab identity exhausted");
        id
    }

    fn reload_config(&mut self) {
        match desktop::home_path().map_or_else(
            || Err(anyhow::anyhow!("home directory is unavailable")),
            |home| ssh_config::Config::load(&home),
        ) {
            Ok(config) => {
                self.config = sync::Arc::new(config);
                self.config_error = None;
            }
            Err(error) => {
                self.config_error = Some(format!("Could not load SSH config: {error:#}"));
            }
        }
        for tab in self
            .tabs
            .iter_mut()
            .chain(std::iter::once(&mut self.composer))
        {
            tab.ui.config = sync::Arc::clone(&self.config);
            tab.ui.config_load_error = self.config_error.clone();
            tab.ui.refresh_profile();
        }
    }

    fn cancel_transient(&mut self) {
        for tab in &mut self.tabs {
            tab.ui.cancel_transient();
        }
        self.composer.ui.cancel_transient();
        self.renaming = None;
    }

    pub fn terminal_focused(&self, ctx: &egui::Context) -> bool {
        !self.composer_open
            && self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.ui.terminal_focused(ctx))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    fn close_shortcut_action(&self) -> Action {
        let Some(tab) = self.tabs.get(self.active) else {
            return Action::None;
        };
        if self.composer_open {
            Action::Select(tab.id)
        } else {
            Action::Close(tab.id)
        }
    }

    pub fn show(&mut self, root: &mut egui::Ui) -> Action {
        self.local_dirty = false;
        let mut navigation = Action::None;
        let mut reorder: Option<(u64, usize)> = None;
        let mut rename_to: Option<(u64, String)> = None;
        let new = egui::KeyboardShortcut::new(
            if cfg!(target_os = "macos") {
                egui::Modifiers::MAC_CMD
            } else {
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT
            },
            egui::Key::T,
        );
        let close = egui::KeyboardShortcut::new(new.modifiers, egui::Key::W);
        if root.input_mut(|input| input.consume_shortcut(&new)) {
            navigation = Action::New;
        }
        if root.input_mut(|input| input.consume_shortcut(&close)) {
            navigation = self.close_shortcut_action();
        }
        egui::Panel::top("connection-tabs")
            .frame(
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .fill(root.visuals().panel_fill),
            )
            .show_inside(root, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
                    ui.spacing_mut().interact_size.y = 28.0;
                    // Hovered buttons grow via expansion and a thicker stroke in
                    // inner_margin. Keep those identical so the strip does not
                    // change height. Button::fill also kills hover, so phase
                    // color is set on the widget visuals instead.
                    let idle_fill = ui.visuals().widgets.inactive.weak_bg_fill;
                    let idle_hover = lift(idle_fill, 32);
                    let selection_stroke = ui.visuals().selection.stroke.color;
                    let stroke_width = ui.visuals().widgets.inactive.bg_stroke.width;
                    {
                        let widgets = &mut ui.visuals_mut().widgets;
                        widgets.inactive.expansion = 0.0;
                        widgets.hovered.expansion = 0.0;
                        widgets.active.expansion = 0.0;
                        widgets.open.expansion = 0.0;
                        widgets.inactive.bg_stroke.width = stroke_width;
                        widgets.hovered.bg_stroke.width = stroke_width;
                        widgets.active.bg_stroke.width = stroke_width;
                        widgets.open.bg_stroke.width = stroke_width;
                    }
                    paint_tab_fills(ui, idle_fill, idle_hover);
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("About").size(14.0))
                                .min_size(egui::vec2(0.0, 28.0))
                                .corner_radius(5.0)
                                .sense(egui::Sense::CLICK),
                        )
                        .clicked()
                    {
                        self.about = true;
                    }
                    ui.with_layout(
                        egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                        |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                            ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
                            paint_tab_fills(ui, idle_fill, idle_hover);
                            let idle_after = time::Duration::from_secs(u64::from(self.idle));
                            let now = time::Instant::now();
                            for tab in &mut self.tabs {
                                let state = tab.client.lock();
                                let phase = state.phase;
                                let seq = state
                                    .view
                                    .as_ref()
                                    .map(crate::snapshot::View::display_seq)
                                    .unwrap_or(0);
                                drop(state);
                                if seq != tab.last_seq || phase != tab.last_phase {
                                    tab.last_seq = seq;
                                    tab.last_phase = phase;
                                    tab.last_output = now;
                                }
                            }
                            for index in 0..self.tabs.len() {
                                let id = self.tabs[index].id;
                                let last_output = self.tabs[index].last_output;
                                let label = self.tabs[index].label.clone();
                                let phase = self.tabs[index].client.phase();
                                ui.push_id(id, |ui| {
                                    if self.renaming.as_ref().is_some_and(|rename| rename.id == id)
                                    {
                                        let rename = self.renaming.as_mut().expect("checked");
                                        let edit = egui::TextEdit::singleline(&mut rename.draft)
                                            .desired_width(180.0)
                                            .font(egui::TextStyle::Button)
                                            .hint_text("session name");
                                        let response = ui.add(edit);
                                        if rename.focus {
                                            response.request_focus();
                                            rename.focus = false;
                                        }
                                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                        let escape =
                                            ui.input(|i| i.key_pressed(egui::Key::Escape));
                                        if escape {
                                            self.renaming = None;
                                        } else if enter {
                                            let draft = self
                                                .renaming
                                                .as_ref()
                                                .expect("checked")
                                                .draft
                                                .trim()
                                                .to_owned();
                                            rename_to = Some((id, draft));
                                            self.renaming = None;
                                        } else if response.lost_focus() {
                                            self.renaming = None;
                                        }
                                        return;
                                    }
                                    let busy = busy_phase(phase);
                                    let mut title = label;
                                    if busy {
                                        title = format!("   {title}");
                                        ui.ctx()
                                            .request_repaint_after(time::Duration::from_millis(50));
                                    }
                                    let selected = !self.composer_open && index == self.active;
                                    let quiet = self.idle > 0
                                        && live_phase(phase)
                                        && now.saturating_duration_since(last_output)
                                            >= idle_after;
                                    if !quiet && self.idle > 0 && live_phase(phase) {
                                        ui.ctx().request_repaint_after(
                                            idle_after.saturating_sub(
                                                now.saturating_duration_since(last_output),
                                            ),
                                        );
                                    }
                                    let color = tab_color(phase, idle_fill, quiet);
                                    let mut text = egui::RichText::new(title).size(16.0).strong();
                                    if phase == desktop::Phase::Failed {
                                        text = text.color(egui::Color32::from_rgb(255, 196, 196));
                                    }
                                    paint_tab_fills(ui, color, lift(color, 32));
                                    if selected {
                                        ui.visuals_mut().selection.bg_fill = lift(color, 50);
                                        ui.visuals_mut().selection.stroke.color =
                                            egui::Color32::from_rgb(250, 250, 250);
                                    }
                                    let button = egui::Button::new(text)
                                        .selected(selected)
                                        .min_size(egui::vec2(0.0, 28.0))
                                        .corner_radius(5.0)
                                        .sense(egui::Sense::CLICK | egui::Sense::DRAG)
                                        .stroke(egui::Stroke::new(
                                            2.0_f32,
                                            if selected {
                                                selection_stroke
                                            } else {
                                                egui::Color32::TRANSPARENT
                                            },
                                        ));
                                    let response = ui.add(button).on_hover_text(
                                        "Click to switch · double-click to rename · drag to reorder",
                                    );
                                    if busy {
                                        let indicator = egui::Rect::from_center_size(
                                            egui::pos2(
                                                response.rect.left() + 14.0,
                                                response.rect.center().y,
                                            ),
                                            egui::vec2(14.0, 14.0),
                                        );
                                        ui::paint_activity_indicator(
                                            ui,
                                            indicator,
                                            ui.ctx().time(),
                                        );
                                    }
                                    response.dnd_set_drag_payload(id);
                                    if response.dragged() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                                        ui.ctx().request_repaint();
                                    }
                                    if let Some(insert_at) = drop_insert_at(
                                        &response,
                                        id,
                                        index,
                                        ui.input(|i| i.pointer.interact_pos()),
                                    ) {
                                        paint_drop_marker(ui, response.rect, insert_at > index);
                                        if let Some(dragged) =
                                            response.dnd_release_payload::<u64>()
                                        {
                                            reorder = Some((*dragged, insert_at));
                                        }
                                    }
                                    if response.double_clicked() {
                                        match phase {
                                            desktop::Phase::Watching => {
                                                self.renaming = Some(SessionRename {
                                                    id,
                                                    draft: self.tabs[index]
                                                        .ui
                                                        .session_name()
                                                        .to_owned(),
                                                    focus: true,
                                                });
                                            }
                                            desktop::Phase::Demo => {
                                                self.notice = Some(
                                                    "The demo has no remote session to rename."
                                                        .to_owned(),
                                                );
                                            }
                                            _ => {
                                                self.notice = Some(
                                                    "Connect before renaming the session."
                                                        .to_owned(),
                                                );
                                            }
                                        }
                                    } else if response.clicked() {
                                        navigation = Action::Select(id);
                                        self.renaming = None;
                                    }
                                });
                            }
                            paint_tab_fills(ui, idle_fill, idle_hover);
                            if self.composer_open {
                                ui.visuals_mut().selection.bg_fill = lift(idle_fill, 50);
                                ui.visuals_mut().selection.stroke.color =
                                    egui::Color32::from_rgb(250, 250, 250);
                            }
                            let add = ui
                                .add_enabled(
                                    self.tabs.len() < MAX_TABS || self.composer_open,
                                    egui::Button::new(egui::RichText::new("+").size(16.0).strong())
                                        .selected(self.composer_open)
                                        .min_size(egui::vec2(28.0, 28.0))
                                        .corner_radius(5.0)
                                        .sense(egui::Sense::CLICK)
                                        .stroke(egui::Stroke::new(
                                            2.0_f32,
                                            if self.composer_open {
                                                selection_stroke
                                            } else {
                                                egui::Color32::TRANSPARENT
                                            },
                                        )),
                                )
                                .on_hover_text("New connection");
                            if add.dnd_hover_payload::<u64>().is_some() {
                                paint_drop_marker(ui, add.rect, false);
                                if let Some(id) = add.dnd_release_payload::<u64>() {
                                    reorder = Some((*id, self.tabs.len()));
                                }
                            }
                            if add.clicked() {
                                navigation = Action::New;
                            }
                            if let Some(ref notice) = self.notice {
                                ui.colored_label(ui.visuals().error_fg_color, notice);
                            }
                        },
                    );
                });
            });
        if self.about {
            self.show_about(root.ctx());
        }
        if root.input(|input| !input.raw.hovered_files.is_empty()) {
            root.ctx().set_cursor_icon(egui::CursorIcon::Copy);
        }
        if self.composer_open {
            self.composer
                .ui
                .take_dropped_files(root.ctx(), &self.composer.client.lock());
        } else if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.ui.take_dropped_files(root.ctx(), &tab.client.lock());
        }
        if let Some((id, insert_at)) = reorder {
            navigation = Action::Reorder { id, insert_at };
        }
        if let Some((id, name)) = rename_to {
            return Action::Tab(id, Box::new(ui::Action::RenameSession(name)));
        }
        if matches!(
            navigation,
            Action::New | Action::Select(_) | Action::Close(_)
        ) {
            // Switching tabs consumes the whole navigation frame. Keyboard and
            // clipboard events collected for the old tab cannot hit the new one.
            return navigation;
        }
        let (id, action) = if self.composer_open || self.tabs.is_empty() {
            self.composer_open = true;
            let action = root
                .push_id(self.composer.id, |root| {
                    self.composer
                        .ui
                        .show(root, &mut self.composer.client.lock())
                })
                .inner;
            ack_painted(&mut self.composer);
            (self.composer.id, action)
        } else {
            let (id, action, renamed) = {
                let tab = &mut self.tabs[self.active];
                let action = root
                    .push_id(tab.id, |root| tab.ui.show(root, &mut tab.client.lock()))
                    .inner;
                ack_painted(tab);
                let renamed = tab.client.take_renamed();
                if let Some(ref name) = renamed {
                    tab.ui.set_session_name(name.clone());
                    tab.label = label(&tab.ui.saved());
                }
                (tab.id, action, renamed)
            };
            if renamed.is_some() {
                self.persist();
            }
            (id, action)
        };
        if !matches!(navigation, Action::None) {
            // Reorder keeps this tab painted so the focused pane stays in
            // egui's used_ids. Skipping a frame would drop keyboard focus
            // while the white border still claimed the pane was selected.
            return navigation;
        }
        Action::Tab(id, Box::new(action))
    }

    /// Apply once, after egui finished its potentially repeated layout passes.
    pub fn apply(&mut self, action: Action, mut clipboard: impl FnMut() -> Option<String>) {
        let nothing_to_do = match &action {
            Action::None => true,
            Action::Tab(_, tab_action) => matches!(**tab_action, ui::Action::None),
            _ => false,
        };
        if nothing_to_do {
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            match action {
                Action::None => {}
                Action::New => {
                    self.open_composer();
                }
                Action::Select(id) => {
                    if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                        self.cancel_transient();
                        self.composer_open = false;
                        self.active = index;
                        self.tabs[index].ui.arm_focus_restore();
                        self.persist();
                    }
                }
                Action::Reorder { id, insert_at } => {
                    self.reorder(id, insert_at);
                    self.persist();
                }
                Action::Close(id) => {
                    if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                        self.cancel_transient();
                        self.tabs.remove(index); // Client::drop invalidates tokens and wakes its worker.
                        if index < self.active {
                            self.active -= 1;
                        }
                        if self.tabs.is_empty() {
                            self.active = 0;
                            self.composer_open = true;
                        } else {
                            self.active = self.active.min(self.tabs.len() - 1);
                        }
                        self.persist();
                    }
                }
                Action::Tab(id, action) => {
                    let mut save = false;
                    let mut follow_input = false;
                    let mut close_after_exit = None;
                    {
                        if self.composer_open
                            && id == self.composer.id
                            && matches!(*action, ui::Action::Connect(_))
                        {
                            self.promote_composer()?;
                        }
                        let tab = if self.composer_open && id == self.composer.id {
                            &mut self.composer
                        } else {
                            let Some(tab) =
                                self.tabs.get_mut(self.active).filter(|tab| tab.id == id)
                            else {
                                return Ok(());
                            };
                            tab
                        };
                        let result = match *action {
                            ui::Action::None => Ok(()),
                            ui::Action::Connect(connection) => {
                                let started = tab.client.connect(connection).map(|()| {
                                    tab.label = label(&tab.ui.saved());
                                    tab.ui.reset_client_size();
                                });
                                // Remember where a successful connection pointed, so
                                // the next start reopens the same form.
                                save = started.is_ok();
                                started
                            }
                            ui::Action::ListSessions(connection) => {
                                tab.client.list_sessions(connection)
                            }
                            ui::Action::CreateSession(connection) => {
                                // A creation size only sets the new session's initial
                                // geometry; tmux owns it from then on.
                                tab.client
                                    .create_session(connection, crate::core::Size::default())
                            }
                            ui::Action::RenameSession(name) => {
                                if name == tab.ui.session_name() {
                                    Ok(())
                                } else {
                                    tab.client.rename_session(core::SessionName::new(name)?)
                                }
                            }
                            ui::Action::Disconnect => {
                                tab.client.disconnect();
                                tab.ui.return_to_form();
                                tab.label = label(&tab.ui.saved());
                                // An empty form is the composer. Don't leave it
                                // as a chip next to +.
                                if tab.label == NEW_CONNECTION {
                                    close_after_exit = Some(id);
                                }
                                Ok(())
                            }
                            // Resolve clipboard reads in place so the whole frame
                            // still reaches the worker as one ordered, atomic batch.
                            ui::Action::Frame(steps) => {
                                let mut actions = Vec::with_capacity(steps.len());
                                for step in steps {
                                    match step {
                                        ui::Step::Send(target, action) => {
                                            if matches!(action, crate::input::Action::SelectPane) {
                                                save = true;
                                            }
                                            actions.push((target, action))
                                        }
                                        ui::Step::RequestPaste(target) => {
                                            if let Some(text) = clipboard()
                                                && let Some(action) = tab.ui.clipboard_paste(
                                                    &tab.client.lock(),
                                                    target,
                                                    &text,
                                                )
                                            {
                                                actions.push((target, action));
                                            }
                                        }
                                    }
                                }
                                if actions.is_empty() {
                                    Ok(())
                                } else {
                                    let sent = tab.client.submit_batch(actions);
                                    follow_input = sent.is_ok();
                                    sent
                                }
                            }
                            ui::Action::ReloadConfig => {
                                self.reload_config();
                                return Ok(());
                            }
                        };
                        if let Err(error) = result {
                            tab.client.lock().error = Some(error.to_string());
                        }
                    }
                    if follow_input {
                        self.echo_until =
                            Some(time::Instant::now() + time::Duration::from_millis(400));
                    }
                    if save {
                        self.persist();
                    }
                    if let Some(id) = close_after_exit
                        && let Some(index) = self.tabs.iter().position(|tab| tab.id == id)
                    {
                        self.cancel_transient();
                        self.tabs.remove(index);
                        if index < self.active {
                            self.active -= 1;
                        }
                        if self.tabs.is_empty() {
                            self.active = 0;
                            self.composer_open = true;
                        } else {
                            self.active = self.active.min(self.tabs.len() - 1);
                        }
                        self.persist();
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.notice = Some(error.to_string());
        }
        self.local_dirty = true;
        (self.wake)();
    }

    /// If wall time jumped while this process's monotonic clock did not, the
    /// machine slept. Wake SSH workers so they can treat the control stream as
    /// lost instead of sitting on "Connected" until the next keystroke.
    pub(crate) fn notice_suspend(&mut self) {
        if !self.suspend_clock.suspended() {
            return;
        }
        self.suspend_clock = reconnect::AliveClock::now();
        for tab in &self.tabs {
            tab.client.nudge();
        }
    }

    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        self.cancel_transient();
        // Save before dropping the tabs: the forms are the thing being saved.
        self.persist();
        self.tabs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_connected_tab_turns_blue() {
        let idle = egui::Color32::from_rgb(40, 40, 40);
        let live = tab_color(desktop::Phase::Watching, idle, false);
        let quiet = tab_color(desktop::Phase::Watching, idle, true);
        assert_ne!(live, quiet);
        assert_eq!(
            tab_color(desktop::Phase::Failed, idle, false),
            tab_color(desktop::Phase::Reconnecting, idle, false),
            "failed uses the same chip fill as reconnecting"
        );
        assert_eq!(
            tab_color(desktop::Phase::Resynchronizing, idle, false),
            live,
            "a layout rebuild is not a yellow reconnect"
        );
        assert!(!busy_phase(desktop::Phase::Resynchronizing));
    }

    #[test]
    fn open_time_formats_compactly() {
        assert_eq!(format_open(8), "8s");
        assert_eq!(format_open(90), "1m");
        assert_eq!(format_open(3720), "1h 2m");
        assert_eq!(format_open(86_400 * 2 + 3_600), "2d 1h");
    }
    #[test]
    fn plus_opens_the_composer_without_a_new_tab() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        assert_eq!(workspace.tabs.len(), 1);
        workspace.apply(Action::New, || None);
        assert_eq!(workspace.tabs.len(), 1);
        assert!(workspace.composer_open);
        assert!(workspace.composer.ui.showing_form());
    }

    #[test]
    fn closing_the_composer_does_not_close_the_hidden_session() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let live = workspace.tabs[0].id;
        workspace.open_composer();
        assert!(matches!(
            workspace.close_shortcut_action(),
            Action::Select(id) if id == live
        ));
        workspace.composer_open = false;
        assert!(matches!(
            workspace.close_shortcut_action(),
            Action::Close(id) if id == live
        ));
    }

    #[test]
    fn connecting_from_the_composer_registers_a_tab() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        workspace.open_composer();
        let composer = workspace.composer.id;
        workspace.promote_composer().unwrap();
        assert_eq!(workspace.tabs.len(), 2);
        assert!(!workspace.composer_open);
        assert_eq!(workspace.tabs[1].id, composer);
        assert_ne!(workspace.composer.id, composer);
    }

    #[test]
    fn exit_closes_an_empty_form_and_keeps_a_named_one() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        workspace.push_idle_tab().unwrap();
        let first = workspace.tabs[0].id;
        workspace.apply(Action::Select(first), || None);
        workspace.apply(Action::Tab(first, Box::new(ui::Action::Disconnect)), || {
            None
        });
        assert_eq!(workspace.tabs.len(), 1);
        assert_ne!(workspace.tabs[0].id, first);

        workspace.tabs[0].ui.restore(store::Tab {
            destination: "dev".into(),
            host: "10.0.0.2".into(),
            session: "work".into(),
            ..store::Tab::default()
        });
        workspace.tabs[0].label = label(&workspace.tabs[0].ui.saved());
        let id = workspace.tabs[0].id;
        workspace.apply(Action::Tab(id, Box::new(ui::Action::Disconnect)), || None);
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.tabs[0].label, "dev / work");
        assert!(workspace.tabs[0].ui.showing_form());
    }

    #[test]
    fn a_tab_without_a_destination_is_labelled_by_host_and_session() {
        assert_eq!(
            label(&store::Tab {
                host: "build.example.test".into(),
                session: "ci".into(),
                ..store::Tab::default()
            }),
            "build.example.test / ci"
        );
        assert_eq!(
            label(&store::Tab {
                destination: "dev".into(),
                host: "10.0.0.2".into(),
                session: "work".into(),
                ..store::Tab::default()
            }),
            "dev / work"
        );
    }

    #[test]
    fn reordering_tabs_keeps_the_active_tab() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let first = workspace.tabs[0].id;
        workspace.push_idle_tab().unwrap();
        let second = workspace.tabs[1].id;
        assert_eq!(workspace.active, 1);
        workspace.apply(
            Action::Reorder {
                id: second,
                insert_at: 0,
            },
            || None,
        );
        assert_eq!(workspace.tabs[0].id, second);
        assert_eq!(workspace.tabs[1].id, first);
        assert_eq!(workspace.active, 0);
        workspace.apply(
            Action::Reorder {
                id: second,
                insert_at: 0,
            },
            || None,
        );
        assert_eq!(workspace.tabs[0].id, second);
    }

    #[test]
    fn new_tab_preserves_existing_session_and_close_only_detaches_its_client() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let original = workspace.tabs[0].id;
        workspace.push_idle_tab().unwrap();
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(workspace.tabs[0].client.phase(), desktop::Phase::Demo);
        assert_eq!(workspace.tabs[1].client.phase(), desktop::Phase::Idle);
        workspace.apply(Action::Close(workspace.tabs[1].id), || None);
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.tabs[0].id, original);
        assert_eq!(workspace.tabs[0].client.phase(), desktop::Phase::Demo);
        workspace.shutdown();
        workspace.shutdown();
        assert!(workspace.tabs.is_empty());
    }
    #[test]
    fn stale_tab_actions_do_not_modify_the_current_tab() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let original = workspace.tabs[0].id;
        workspace.push_idle_tab().unwrap();
        workspace.apply(
            Action::Tab(original, Box::new(ui::Action::Disconnect)),
            || None,
        );
        assert_eq!(workspace.tabs[0].client.phase(), desktop::Phase::Demo);
        assert_eq!(workspace.tabs[1].client.phase(), desktop::Phase::Idle);
    }
    #[test]
    fn saved_tabs_resume_their_saved_host_and_session_automatically() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let file = directory.join("workspace.conf");
        store::save(
            &file,
            &store::Workspace {
                tabs: vec![
                    store::Tab {
                        destination: "dev".into(),
                        host: "10.0.0.2".into(),
                        user: "alice".into(),
                        session: "work".into(),
                        port: 2222,
                        known_hosts: "/tmp/known_hosts".into(),
                        history: 300,
                        interactive: true,
                        reconnect: true,
                        ..store::Tab::default()
                    },
                    store::Tab {
                        host: "build.example.test".into(),
                        user: "bob".into(),
                        session: "ci".into(),
                        port: 22,
                        known_hosts: "/tmp/known_hosts".into(),
                        history: 200,
                        ..store::Tab::default()
                    },
                ],
                active: 1,
                restore_tabs: true,
                fps: store::DEFAULT_FPS,
                idle: store::DEFAULT_IDLE,
                open_secs: 0,
            },
        )
        .unwrap();

        let mut workspace = idle_workspace(Some(file.clone()));
        let mut resumed = Vec::new();
        assert!(
            workspace
                .restore_with(
                    |_, _| dialog::BrokenStore::Exit,
                    |_, connection| {
                        resumed.push((
                            connection.options.host,
                            connection.session.as_str().to_owned(),
                        ));
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert!(workspace.restore_tabs);
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(workspace.active, 1);
        assert_eq!(
            resumed,
            [
                ("10.0.0.2".into(), "work".into()),
                ("build.example.test".into(), "ci".into())
            ]
        );
        // The injected connector keeps this test off the network. Opening the
        // terminal here proves production startup will show connection progress
        // and then the restored tmux view, not the server chooser.
        for tab in &workspace.tabs {
            assert!(!tab.ui.showing_form());
        }
        assert_eq!(workspace.tabs[0].label, "dev / work");
        assert_eq!(workspace.tabs[1].label, "build.example.test / ci");
        assert!(!workspace.composer_open);
        workspace.persist();
        let reloaded = store::load(&file).unwrap().unwrap();
        assert_eq!(reloaded.tabs.len(), 2);
        assert_eq!(reloaded.tabs[0].host, "10.0.0.2");
        assert_eq!(reloaded.tabs[0].port, 2222);
        assert_eq!(reloaded.tabs[0].session, "work");
        assert_eq!(reloaded.tabs[1].session, "ci");
        assert_eq!(reloaded.active, 1);

        // The native lifecycle can request shutdown more than once. A later
        // call must not replace the first call's saved tabs with the cleared
        // in-memory list.
        workspace.shutdown();
        workspace.shutdown();
        assert_eq!(store::load(&file).unwrap().unwrap().tabs.len(), 2);

        let mut reopened = idle_workspace(Some(file));
        let mut resumed_again = 0;
        assert!(
            reopened
                .restore_with(
                    |_, _| dialog::BrokenStore::Exit,
                    |_, _| {
                        resumed_again += 1;
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert_eq!(reopened.tabs.len(), 2);
        assert_eq!(resumed_again, 2);
        assert_eq!(reopened.active, 1);
        assert!(!reopened.composer_open);
    }

    #[test]
    fn one_open_tab_survives_shutdown_and_the_next_startup() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-one-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let file = directory.join("workspace.conf");

        let mut closing = idle_workspace(Some(file.clone()));
        closing.push_idle_tab().unwrap();
        let saved = store::Tab {
            destination: "dev".into(),
            host: "dev.example.test".into(),
            user: "alice".into(),
            session: "work".into(),
            window: Some(0),
            pane: Some(1),
            port: 22,
            known_hosts: "/tmp/known_hosts".into(),
            history: store::DEFAULT_HISTORY,
            interactive: true,
            reconnect: true,
            ..store::Tab::default()
        };
        closing.tabs[0].label = label(&saved);
        closing.tabs[0].ui.restore(saved);
        closing.shutdown();
        closing.shutdown();

        let on_disk = store::load(&file).unwrap().unwrap();
        assert!(on_disk.restore_tabs);
        assert_eq!(on_disk.tabs.len(), 1);
        assert_eq!(on_disk.tabs[0].destination, "dev");
        assert_eq!(on_disk.tabs[0].window, Some(0));
        assert_eq!(on_disk.tabs[0].pane, Some(1));

        let mut started = idle_workspace(Some(file));
        let mut resumed = None;
        assert!(
            started
                .restore_with(
                    |_, _| dialog::BrokenStore::Exit,
                    |_, connection| {
                        resumed = Some((
                            connection.options.host,
                            connection.session.as_str().to_owned(),
                        ));
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert_eq!(started.tabs.len(), 1);
        assert_eq!(started.tabs[0].ui.saved().window, Some(0));
        assert_eq!(started.tabs[0].ui.saved().pane, Some(1));
        assert_eq!(started.tabs[0].label, "dev / work");
        assert_eq!(resumed, Some(("dev.example.test".into(), "work".into())));
        assert!(!started.tabs[0].ui.showing_form());
        assert!(!started.composer_open);
    }

    #[test]
    fn an_incomplete_saved_tab_stays_on_its_form_with_an_error() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-incomplete-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let file = directory.join("workspace.conf");
        store::save(
            &file,
            &store::Workspace {
                tabs: vec![store::Tab {
                    destination: "dev".into(),
                    host: "dev.example.test".into(),
                    known_hosts: "/tmp/known_hosts".into(),
                    ..store::Tab::default()
                }],
                ..store::Workspace::default()
            },
        )
        .unwrap();

        let mut workspace = idle_workspace(Some(file));
        let mut attempts = 0;
        assert!(
            workspace
                .restore_with(
                    |_, _| dialog::BrokenStore::Exit,
                    |_, _| {
                        attempts += 1;
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert_eq!(attempts, 0);
        assert!(workspace.tabs[0].ui.showing_form());
        assert!(
            workspace.tabs[0]
                .client
                .error()
                .is_some_and(|error| error.contains("choose a tmux session"))
        );
    }

    #[test]
    fn disabled_tab_restore_starts_with_an_empty_workspace() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-no-restore-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let file = directory.join("workspace.conf");
        store::save(
            &file,
            &store::Workspace {
                tabs: vec![store::Tab {
                    destination: "dev".into(),
                    host: "dev.example.test".into(),
                    ..store::Tab::default()
                }],
                restore_tabs: false,
                ..store::Workspace::default()
            },
        )
        .unwrap();

        let mut workspace = idle_workspace(Some(file));
        assert!(workspace.restore(|_, _| dialog::BrokenStore::Exit).unwrap());
        assert!(!workspace.restore_tabs);
        assert!(workspace.tabs.is_empty());
        assert!(workspace.composer_open);
    }

    fn idle_workspace(store: Option<path::PathBuf>) -> Workspace {
        let wake: Wake = sync::Arc::new(|| {});
        Workspace {
            tabs: Vec::new(),
            active: 0,
            composer: spawn_tab(
                1,
                sync::Arc::clone(&wake),
                sync::Arc::new(ssh_config::Config::default()),
                None,
            )
            .unwrap(),
            composer_open: true,
            next: 2,
            wake,
            config: sync::Arc::new(ssh_config::Config::default()),
            config_error: None,
            notice: None,
            store,
            fps: store::DEFAULT_FPS,
            idle: store::DEFAULT_IDLE,
            restore_tabs: true,
            about: false,
            about_icon: None,
            open_secs: 0,
            session_started: time::Instant::now(),
            echo_until: None,
            suspend_clock: reconnect::AliveClock::now(),
            local_dirty: false,
            shut_down: false,
            renaming: None,
        }
    }

    fn broken_workspace(file: path::PathBuf) -> Workspace {
        idle_workspace(Some(file))
    }

    #[test]
    fn an_unreadable_saved_workspace_is_left_alone_when_the_user_exits() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-bad-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("workspace.conf");
        let original = "[tab]\nport not-a-number\n";
        std::fs::write(&file, original).unwrap();
        let mut workspace = broken_workspace(file.clone());
        assert!(!workspace.restore(|_, _| dialog::BrokenStore::Exit).unwrap());
        assert!(workspace.tabs.is_empty());
        workspace.open_composer();
        workspace.persist();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn clearing_an_unreadable_saved_workspace_deletes_the_file() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-clear-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("workspace.conf");
        std::fs::write(&file, "[tab]\nport not-a-number\n").unwrap();
        let mut workspace = broken_workspace(file.clone());
        assert!(
            workspace
                .restore(|_, _| dialog::BrokenStore::Clear)
                .unwrap()
        );
        assert!(workspace.tabs.is_empty());
        assert!(!file.exists(), "Clear must delete the unreadable file");
        workspace.push_idle_tab().unwrap();
        workspace.persist();
        assert!(store::load(&file).unwrap().is_some());
    }

    #[test]
    fn connections_and_tabs_are_bounded() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        for _ in 1..MAX_TABS {
            workspace.push_idle_tab().unwrap();
        }
        assert!(workspace.push_idle_tab().is_err());
    }
    #[test]
    fn native_smoke_geometry() {
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        for pass in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 760.0),
                )),
                time: Some(pass as f64 / 60.0),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |root| {
                workspace.show(root);
            });
        }
        let status_h = egui::containers::panel::PanelState::load(&ctx, egui::Id::new("status"))
            .map(|state| state.rect.height())
            .unwrap_or(0.0);
        assert!(
            (1.0..48.0).contains(&status_h),
            "status bar must not eat the window, height was {status_h}"
        );
        let (start, end) = workspace.tabs[0].ui.smoke_selection(&ctx);
        assert!(start.x > 0.0 && start.x < end.x);
        if let Some(path) = std::env::var_os("STARCOM_SMOKE_GEOMETRY") {
            std::fs::write(
                path,
                format!(
                    "{{\"start\":[{},{}],\"end\":[{},{}]}}",
                    start.x, start.y, end.x, end.y
                ),
            )
            .unwrap();
        }
    }

    #[test]
    fn sending_input_raises_the_remote_paint_rate() {
        let workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let idle = workspace.repaint_interval();
        assert_eq!(workspace.paint_interval(), idle);
        let mut workspace = workspace;
        workspace.echo_until = Some(time::Instant::now() + time::Duration::from_millis(400));
        assert_eq!(workspace.paint_interval(), time::Duration::from_millis(50));
        workspace.echo_until = Some(time::Instant::now() - time::Duration::from_millis(1));
        assert_eq!(workspace.paint_interval(), idle);
    }

    fn paint(workspace: &mut Workspace) {
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 760.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |root| {
            workspace.show(root);
        });
    }

    #[test]
    fn hidden_terminal_updates_do_not_repaint_the_selected_terminal() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        assert!(workspace.remote_changed());
        assert!(
            workspace.remote_changed(),
            "pending work stays pending until the terminal is painted"
        );
        paint(&mut workspace);
        assert!(
            !workspace.remote_changed(),
            "duplicate wakes are discarded after paint"
        );

        workspace.push_idle_tab().unwrap();
        workspace.active = 0;
        workspace.tabs[1].client.demo().unwrap();
        assert!(
            workspace.remote_changed(),
            "a hidden tab's phase change updates its visible chip"
        );
        assert!(!workspace.remote_changed());

        workspace.tabs[1].client.demo().unwrap();
        assert!(
            !workspace.remote_changed(),
            "new hidden terminal contents do not repaint the selected terminal"
        );
        workspace.tabs[0].client.demo().unwrap();
        assert!(
            workspace.remote_changed(),
            "new selected terminal contents do repaint"
        );
        paint(&mut workspace);
        assert!(
            !workspace.remote_changed(),
            "selected contents stay pending only until show paints"
        );
    }

    #[test]
    fn idle_frames_do_not_request_another_repaint() {
        let count = sync::Arc::new(sync::atomic::AtomicUsize::new(0));
        let wake = sync::Arc::clone(&count);
        let mut workspace = Workspace::new(
            sync::Arc::new(move || {
                wake.fetch_add(1, sync::atomic::Ordering::Relaxed);
            }),
            desktop::Startup::Demo,
        )
        .unwrap();
        let before = count.load(sync::atomic::Ordering::Relaxed);
        workspace.apply(
            Action::Tab(workspace.tabs[0].id, Box::new(ui::Action::None)),
            || None,
        );
        assert_eq!(count.load(sync::atomic::Ordering::Relaxed), before);
    }
}
