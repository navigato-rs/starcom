//! Logical tabs own independent UI state while tabs on one resolved server
//! share a single SSH/tmux control client and reconstructed managed-session view.

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenameKey {
    Keep,
    Submit,
    Cancel,
}

/// Enter confirms even when the field also loses focus (egui single-line
/// TextEdit surrenders on Enter). Escape and click-away cancel.
fn rename_key_outcome(enter: bool, escape: bool, lost_focus: bool) -> RenameKey {
    if escape {
        RenameKey::Cancel
    } else if enter {
        RenameKey::Submit
    } else if lost_focus {
        RenameKey::Cancel
    } else {
        RenameKey::Keep
    }
}

struct Tab {
    id: u64,
    label: String,
    client: sync::Arc<desktop::Client>,
    server: Option<desktop::ServerKey>,
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
    /// After a keystroke, remote paint may run faster until this instant —
    /// one idle refresh interval, not a fixed window. None when idle.
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
    /// Survives extra egui layout passes in the same frame. The last `show`
    /// would otherwise overwrite a RenameSession with terminal Enter.
    submitted_rename: Option<(u64, String)>,
}

/// The server lives in the status bar; tabs stay compact and name only the
/// session the user switches between.
fn label(tab: &store::Tab) -> String {
    match tab.session.trim() {
        "" => NEW_CONNECTION.to_owned(),
        session => session.to_owned(),
    }
}

fn same_endpoint(left: &store::Tab, right: &store::Tab) -> bool {
    left.host.trim() == right.host.trim()
        && left.user.trim() == right.user.trim()
        && left.port == right.port
        && left.socket.trim() == right.socket.trim()
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
        client: sync::Arc::new(desktop::Client::new(wake)?),
        server: None,
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

fn logical_phase(tab: &Tab, state: &desktop::State) -> desktop::Phase {
    if state.phase != desktop::Phase::Demo
        && live_phase(state.phase)
        && !tab.ui.window_available(state)
    {
        desktop::Phase::Failed
    } else {
        state.phase
    }
}

fn tab_phase(tab: &Tab) -> desktop::Phase {
    let state = tab.client.lock();
    logical_phase(tab, &state)
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
        desktop::Phase::Failed | desktop::Phase::Disconnected => {
            egui::Color32::from_rgb(148, 40, 40)
        }
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

fn compact_tab_title(label: &str) -> String {
    const MAX_CHARS: usize = 24;
    let mut chars = label.chars();
    let mut title: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        title.push('…');
    }
    title
}

fn tab_width(label: &str, renaming: bool, busy: bool) -> f32 {
    if renaming {
        return 180.0;
    }
    let text = compact_tab_title(label);
    20.0 + text.chars().count() as f32 * 9.0 + if busy { 21.0 } else { 0.0 }
}

fn visible_tab_range(widths: &[f32], anchor: usize, budget: f32) -> std::ops::Range<usize> {
    if widths.is_empty() {
        return 0..0;
    }
    let anchor = anchor.min(widths.len() - 1);
    let mut start = anchor;
    let mut end = anchor + 1;
    let mut used = widths[anchor];
    loop {
        let left = start.checked_sub(1);
        let right = (end < widths.len()).then_some(end);
        let next = match (left, right) {
            (Some(left), Some(right)) => {
                if anchor - left <= right - anchor {
                    Some((left, true))
                } else {
                    Some((right, false))
                }
            }
            (Some(left), None) => Some((left, true)),
            (None, Some(right)) => Some((right, false)),
            (None, None) => None,
        };
        let Some((index, is_left)) = next else {
            break;
        };
        if used + widths[index] > budget && end > start {
            break;
        }
        used += widths[index];
        if is_left {
            start = index;
        } else {
            end = index + 1;
        }
    }
    start..end
}

fn paint_animated_dashed_rect(ui: &egui::Ui, rect: egui::Rect, stroke: egui::Stroke) {
    const DASH: f32 = 4.0;
    const GAP: f32 = 3.0;
    const SPEED: f32 = 14.0;
    let dash_offset = (ui.ctx().time() as f32 * SPEED) % (DASH + GAP);
    let path = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    ui.painter().extend(egui::Shape::dashed_line_with_offset(
        &path,
        stroke,
        &[DASH],
        &[GAP],
        dash_offset,
    ));
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
            submitted_rename: None,
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
        let saved_active = saved.active;
        let mut active_restored = false;
        let mut active_failure = false;
        if self.restore_tabs {
            for (saved_index, tab) in saved.tabs.into_iter().enumerate() {
                if tab.legacy {
                    // V0.3 saved one record per remote tmux session. Runtime
                    // discovery now treats every non-managed session equally,
                    // so these stale local compatibility records carry no
                    // selection or lifecycle authority.
                    continue;
                }
                let id = self.alloc_id();
                let mut restored = spawn_tab(
                    id,
                    sync::Arc::clone(&self.wake),
                    sync::Arc::clone(&self.config),
                    self.config_error.clone(),
                )?;
                restored.label = label(&tab);
                restored.ui.restore(tab);
                let result = restored.ui.resume().and_then(|connection| {
                    let key = connection.server_key();
                    if let Some(existing) = self
                        .tabs
                        .iter()
                        .find(|tab| tab.server.as_ref() == Some(&key))
                    {
                        existing.client.ensure_policy(connection)?;
                        restored.client = sync::Arc::clone(&existing.client);
                    } else {
                        resume(&restored.client, connection)?;
                    }
                    restored.server = Some(key);
                    Ok(())
                });
                match result {
                    Ok(()) => {
                        if saved_index == saved_active {
                            self.active = self.tabs.len();
                            active_restored = true;
                        }
                        self.tabs.push(restored);
                    }
                    Err(error) => {
                        let message = format!("Could not resume {}: {error}", restored.label);
                        restored.client.disconnect();
                        restored.client.lock().error = Some(message.clone());
                        restored.ui.return_to_form();
                        if saved_index == saved_active {
                            // Keep the failed destination available for repair,
                            // but only on `+`, never as an empty registered tab.
                            self.composer = restored;
                            active_failure = true;
                        } else {
                            self.notice = Some(message);
                        }
                    }
                }
            }
        }
        if !active_restored {
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
        }
        self.composer_open = active_failure || self.tabs.is_empty();
        self.fps = store::clamp_fps(saved.fps);
        self.idle = saved.idle.min(store::MAX_IDLE);
        self.open_secs = saved.open_secs.min(store::MAX_OPEN_SECS);
        Ok(true)
    }

    pub(crate) fn repaint_interval(&self) -> time::Duration {
        time::Duration::from_secs_f64(1.0 / f64::from(store::clamp_fps(self.fps)))
    }

    /// Idle remote paint uses `fps` from the saved workspace. After a key, the
    /// next idle slot may run at up to 20 fps so echo is not a full cycle late.
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

    fn arm_echo(&mut self) {
        self.echo_until = Some(time::Instant::now() + self.repaint_interval());
    }

    /// Report whether anything currently visible changed. Worker revisions for
    /// the selected terminal stay pending until `show` paints it; consuming
    /// them here made a later coalesced wake look like a duplicate and skip
    /// the real frame. Hidden-tab output still does not repaint the selected
    /// terminal.
    pub(crate) fn remote_changed(&mut self) -> bool {
        let mut repaint = self.local_dirty;
        let mut echo = false;
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
                .map(|view| tab.ui.display_seq(view))
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
            if visible && display_changed {
                echo = true;
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
        if echo {
            self.arm_echo();
        }
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
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .take(store::MAX_TABS)
            .map(|tab| tab.ui.saved())
            .collect();
        let saved = store::Workspace {
            tabs,
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
        // Re-read ~/.ssh/config for this form only. Open sessions keep the
        // endpoint they already resolved.
        self.read_ssh_config();
        self.composer.ui.config = sync::Arc::clone(&self.config);
        self.composer.ui.config_load_error = self.config_error.clone();
        self.composer.ui.refresh_profile();
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

    fn finish_tab_removal(&mut self, index: usize) {
        if index < self.active {
            self.active -= 1;
        }
        if self.tabs.is_empty() {
            self.active = 0;
            self.composer_open = true;
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
        }
    }

    fn remove_tab(&mut self, index: usize) {
        self.cancel_transient();
        // The shared client drops only with its final logical tab; that drop
        // invalidates tokens and wakes the one server worker.
        self.tabs.remove(index);
        self.finish_tab_removal(index);
    }

    /// Undo a composer promotion when starting the request itself failed. Once
    /// a worker accepted an attachment, its registered tab remains until Exit.
    fn return_tab_to_composer(&mut self, index: usize) {
        self.cancel_transient();
        let mut tab = self.tabs.remove(index);
        let error = tab.client.error();
        tab.client.disconnect();
        if let Some(error) = error {
            tab.client.lock().error = Some(error);
        }
        tab.ui.return_to_form();
        self.finish_tab_removal(index);
        self.composer = tab;
        self.composer_open = true;
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.checked_add(1).expect("tab identity exhausted");
        id
    }

    fn read_ssh_config(&mut self) {
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
    }

    fn reload_config(&mut self) {
        self.read_ssh_config();
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
        self.submitted_rename = None;
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

    fn apply_renamed_session(&mut self) {
        let mut events = Vec::new();
        for index in 0..self.tabs.len() {
            if self.tabs[..index]
                .iter()
                .any(|tab| sync::Arc::ptr_eq(&tab.client, &self.tabs[index].client))
            {
                continue;
            }
            if let Some((window, name)) = self.tabs[index]
                .client
                .take_renamed()
                .or_else(|| self.tabs[index].client.take_rename_revert())
            {
                events.push((sync::Arc::clone(&self.tabs[index].client), window, name));
            }
        }
        let mut changed = false;
        for (client, window, name) in events {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| {
                sync::Arc::ptr_eq(&tab.client, &client) && tab.ui.current_window() == Some(window)
            }) && name != tab.ui.session_name()
            {
                tab.ui.set_session_name(name);
                tab.label = label(&tab.ui.saved());
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
    }

    fn apply_remote_window_names(&mut self) {
        let mut changed = false;
        for tab in &mut self.tabs {
            let Some(window) = tab.ui.current_window() else {
                continue;
            };
            let name = {
                let state = tab.client.lock();
                state
                    .view
                    .as_ref()
                    .and_then(|view| view.window_name(window))
                    .map(str::to_owned)
            };
            if let Some(name) = name
                && name != tab.ui.session_name()
            {
                tab.ui.set_session_name(name);
                tab.label = label(&tab.ui.saved());
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
    }

    fn apply_moved_session(&mut self) -> anyhow::Result<()> {
        let moved = self.tabs.iter().find_map(|tab| {
            tab.client
                .take_moved_session()
                .map(|moved| (sync::Arc::clone(&tab.client), moved))
        });
        let Some((client, moved)) = moved else {
            return Ok(());
        };
        let source = self
            .tabs
            .iter()
            .position(|tab| {
                sync::Arc::ptr_eq(&tab.client, &client)
                    && tab.ui.current_window() == Some(moved.source)
            })
            .context("the source logical session disappeared during its pane move")?;
        anyhow::ensure!(
            self.tabs.len() < MAX_TABS,
            "the pane moved, but the workspace already has {MAX_TABS} tabs; open its '{}' session manually",
            moved.name
        );

        let mut saved = self.tabs[source].ui.saved();
        saved.session.clone_from(&moved.name);
        saved.window = Some(moved.window.0);
        saved.pane = None;
        let mut tab = spawn_tab(
            self.alloc_id(),
            sync::Arc::clone(&self.wake),
            sync::Arc::clone(&self.config),
            self.config_error.clone(),
        )?;
        tab.label = label(&saved);
        tab.ui.restore(saved);
        tab.client = client;
        tab.server = self.tabs[source].server.clone();

        let insert_at = source + 1;
        self.tabs.insert(insert_at, tab);
        self.active = insert_at;
        self.composer_open = false;
        self.renaming = Some(SessionRename {
            id: self.tabs[self.active].id,
            draft: self.tabs[self.active].ui.session_name().to_owned(),
            focus: true,
        });
        self.persist();
        Ok(())
    }

    pub fn show(&mut self, root: &mut egui::Ui) -> Action {
        if let Err(error) = self.apply_moved_session() {
            self.notice = Some(error.to_string());
        }
        if let Some(migration) = self.composer.client.take_migrated() {
            self.composer.ui.migration_completed(&migration);
        }
        self.local_dirty = false;
        self.apply_renamed_session();
        self.apply_remote_window_names();
        let mut navigation = Action::None;
        let mut reorder: Option<(u64, usize)> = None;
        let mut rename_to: Option<(u64, String)> = None;
        let composer_server = self
            .composer
            .ui
            .server_connection()
            .ok()
            .map(|connection| connection.server_key());
        let connected_servers = self
            .tabs
            .iter()
            .filter(|tab| live_phase(tab.client.phase()))
            .map(|tab| tab.ui.server_name().trim().to_owned())
            .filter(|server| !server.is_empty())
            .collect();
        self.composer.ui.set_connected_servers(connected_servers);
        let unavailable = self
            .tabs
            .iter()
            .filter(|tab| {
                matches!(
                    tab.client.phase(),
                    desktop::Phase::Connecting
                        | desktop::Phase::Watching
                        | desktop::Phase::Resynchronizing
                        | desktop::Phase::Reconnecting
                ) && composer_server
                    .as_ref()
                    .is_some_and(|server| tab.server.as_ref() == Some(server))
            })
            .map(|tab| tab.ui.session_name().to_owned())
            .filter(|session| !session.is_empty())
            .collect();
        self.composer.ui.set_unavailable_sessions(unavailable);
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
                    .inner_margin(egui::Margin {
                        left: 6,
                        right: 6,
                        top: 4,
                        bottom: 0,
                    })
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
                        egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(false),
                        |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                            ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
                            paint_tab_fills(ui, idle_fill, idle_hover);
                            let idle_after = time::Duration::from_secs(u64::from(self.idle));
                            let now = time::Instant::now();
                            for tab in &mut self.tabs {
                                let state = tab.client.lock();
                                let phase = logical_phase(tab, &state);
                                let seq = state
                                    .view
                                    .as_ref()
                                    .map(|view| tab.ui.display_seq(view))
                                    .unwrap_or(0);
                                drop(state);
                                if seq != tab.last_seq || phase != tab.last_phase {
                                    tab.last_seq = seq;
                                    tab.last_phase = phase;
                                    tab.last_output = now;
                                }
                            }
                            let anchor = self
                                .renaming
                                .as_ref()
                                .and_then(|rename| {
                                    self.tabs.iter().position(|tab| tab.id == rename.id)
                                })
                                .unwrap_or(self.active);
                            let widths: Vec<_> = self
                                .tabs
                                .iter()
                                .map(|tab| {
                                    tab_width(
                                        &tab.label,
                                        self.renaming
                                            .as_ref()
                                            .is_some_and(|rename| rename.id == tab.id),
                                        busy_phase(tab_phase(tab)),
                                    ) + ui.spacing().item_spacing.x
                                })
                                .collect();
                            let tab_budget = (ui.available_width() - 34.0).max(1.0);
                            let overflow = widths.iter().sum::<f32>() > tab_budget;
                            let range = if overflow {
                                visible_tab_range(&widths, anchor, (tab_budget - 92.0).max(1.0))
                            } else {
                                0..self.tabs.len()
                            };
                            if range.start > 0 {
                                let hidden = range.start;
                                if ui
                                    .add(
                                        egui::Button::new(format!("◀ {hidden}"))
                                            .min_size(egui::vec2(44.0, 32.0)),
                                    )
                                    .on_hover_text(format!("Show {hidden} hidden tabs"))
                                    .clicked()
                                {
                                    navigation = Action::Select(self.tabs[range.start - 1].id);
                                }
                            }
                            let mut tab_chrome = Vec::new();
                            for index in range.clone() {
                                let id = self.tabs[index].id;
                                let last_output = self.tabs[index].last_output;
                                let label = self.tabs[index].label.clone();
                                let server = self.tabs[index].ui.server_name().to_owned();
                                let phase = tab_phase(&self.tabs[index]);
                                let option_names = {
                                    let state = self.tabs[index].client.lock();
                                    self.tabs[index].ui.option_names(&state)
                                };
                                ui.push_id(id, |ui| {
                                    if self.renaming.as_ref().is_some_and(|rename| rename.id == id)
                                    {
                                        let rename = self.renaming.as_mut().expect("checked");
                                        let edit = egui::TextEdit::singleline(&mut rename.draft)
                                            .desired_width(180.0)
                                            .font(egui::TextStyle::Button)
                                            .hint_text("session name");
                                        let mut output = edit.show(ui);
                                        let response = &output.response;
                                        if rename.focus {
                                            response.request_focus();
                                            output.state.cursor.set_char_range(Some(
                                                egui::text::CCursorRange::two(
                                                    egui::text::CCursor::new(0),
                                                    egui::text::CCursor::new(
                                                        rename.draft.chars().count(),
                                                    ),
                                                ),
                                            ));
                                            output.state.store(ui.ctx(), response.id);
                                            rename.focus = false;
                                        }
                                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                        match rename_key_outcome(
                                            enter,
                                            escape,
                                            response.lost_focus(),
                                        ) {
                                            RenameKey::Submit => {
                                                let draft = self
                                                    .renaming
                                                    .as_ref()
                                                    .expect("checked")
                                                    .draft
                                                    .trim()
                                                    .to_owned();
                                                rename_to = Some((id, draft));
                                                self.renaming = None;
                                            }
                                            RenameKey::Cancel => self.renaming = None,
                                            RenameKey::Keep => {}
                                        }
                                        return;
                                    }
                                    let busy = busy_phase(phase);
                                    let mut title = compact_tab_title(&label);
                                    if busy {
                                        title = format!("   {title}");
                                        ui.ctx()
                                            .request_repaint_after(time::Duration::from_millis(50));
                                    }
                                    let selected = !self.composer_open && index == self.active;
                                    let quiet = self.idle > 0
                                        && live_phase(phase)
                                        && now.saturating_duration_since(last_output) >= idle_after;
                                    if !quiet && self.idle > 0 && live_phase(phase) {
                                        ui.ctx().request_repaint_after(idle_after.saturating_sub(
                                            now.saturating_duration_since(last_output),
                                        ));
                                    }
                                    let color = tab_color(phase, idle_fill, quiet);
                                    let mut text = egui::RichText::new(title).size(16.0).strong();
                                    if matches!(
                                        phase,
                                        desktop::Phase::Failed | desktop::Phase::Disconnected
                                    ) {
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
                                        .min_size(egui::vec2(0.0, 32.0))
                                        .corner_radius(egui::CornerRadius {
                                            nw: 6,
                                            ne: 6,
                                            sw: 0,
                                            se: 0,
                                        })
                                        .sense(egui::Sense::CLICK | egui::Sense::DRAG)
                                        .stroke(egui::Stroke::new(
                                            2.0_f32,
                                            if selected {
                                                selection_stroke
                                            } else {
                                                egui::Color32::TRANSPARENT
                                            },
                                        ));
                                    let hint = if phase == desktop::Phase::Watching {
                                        "Click to switch · double-click to rename · drag to reorder"
                                    } else {
                                        "Click to switch · drag to reorder"
                                    };
                                    let response = ui.add(button).on_hover_ui(|ui| {
                                        if !server.is_empty() {
                                            ui.label(egui::RichText::new(&server).strong());
                                        }
                                        if !option_names.is_empty() {
                                            ui.separator();
                                            ui.weak(format!(
                                                "{} window options",
                                                option_names.len()
                                            ));
                                            for name in &option_names {
                                                ui.label(name);
                                            }
                                        }
                                        ui.separator();
                                        ui.weak(hint);
                                    });
                                    tab_chrome.push((
                                        id,
                                        response.rect,
                                        self.tabs[index].ui.saved(),
                                        response.hovered(),
                                    ));
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
                                        if let Some(dragged) = response.dnd_release_payload::<u64>()
                                        {
                                            reorder = Some((*dragged, insert_at));
                                        }
                                    }
                                    if response.double_clicked()
                                        && phase == desktop::Phase::Watching
                                        && self.tabs[index].ui.saved().interactive
                                    {
                                        self.notice = None;
                                        ui.ctx().memory_mut(|memory| {
                                            if let Some(focused) = memory.focused() {
                                                memory.surrender_focus(focused);
                                            }
                                        });
                                        self.renaming = Some(SessionRename {
                                            id,
                                            draft: self.tabs[index].ui.session_name().to_owned(),
                                            focus: true,
                                        });
                                        // The editor replaces this chip on the
                                        // next paint. The double-click frame
                                        // itself did not contain a TextEdit,
                                        // so egui has no widget-driven reason
                                        // to schedule that paint for us.
                                        ui.ctx().request_repaint();
                                    } else if response.clicked() || response.double_clicked() {
                                        navigation = Action::Select(id);
                                        self.renaming = None;
                                    }
                                });
                            }
                            if let Some((hovered, _, endpoint, _)) =
                                tab_chrome.iter().find(|(_, _, _, hovered)| *hovered)
                            {
                                let stroke = egui::Stroke::new(1.0_f32, selection_stroke);
                                let mut animated = false;
                                for (id, rect, sibling, _) in &tab_chrome {
                                    if id != hovered && same_endpoint(endpoint, sibling) {
                                        paint_animated_dashed_rect(ui, rect.shrink(1.0), stroke);
                                        animated = true;
                                    }
                                }
                                if animated {
                                    ui.ctx()
                                        .request_repaint_after(time::Duration::from_millis(33));
                                }
                            }
                            let hidden_right = self.tabs.len().saturating_sub(range.end);
                            if hidden_right > 0
                                && ui
                                    .add(
                                        egui::Button::new(format!("{hidden_right} ▶"))
                                            .min_size(egui::vec2(44.0, 32.0)),
                                    )
                                    .on_hover_text(format!("Show {hidden_right} hidden tabs"))
                                    .clicked()
                            {
                                navigation = Action::Select(self.tabs[range.end].id);
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
                                        .min_size(egui::vec2(30.0, 32.0))
                                        .corner_radius(egui::CornerRadius {
                                            nw: 6,
                                            ne: 6,
                                            sw: 0,
                                            se: 0,
                                        })
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
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(notice.as_str())
                                            .color(ui.visuals().error_fg_color),
                                    )
                                    .truncate(),
                                );
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
            self.submitted_rename = Some((id, name));
        }
        if let Some((id, name)) = self.submitted_rename.clone() {
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
            let tab = &mut self.tabs[self.active];
            let renaming = self
                .renaming
                .as_ref()
                .is_some_and(|rename| rename.id == tab.id);
            let action = root
                .push_id(tab.id, |root| {
                    if renaming {
                        tab.ui
                            .show_without_terminal_focus(root, &mut tab.client.lock())
                    } else {
                        tab.ui.show(root, &mut tab.client.lock())
                    }
                })
                .inner;
            ack_painted(tab);
            (tab.id, action)
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
                        self.remove_tab(index);
                        self.persist();
                    }
                }
                Action::Tab(id, action) => {
                    let mut save = false;
                    let mut follow_input = false;
                    let mut close_after_exit = None;
                    let mut return_to_composer = false;
                    let mut promoted_composer = false;
                    let can_add_session = self.tabs.len() < MAX_TABS;
                    let composer_connect = self.composer_open
                        && id == self.composer.id
                        && matches!(action.as_ref(), ui::Action::Connect(_));
                    let composer_create = self.composer_open
                        && id == self.composer.id
                        && matches!(action.as_ref(), ui::Action::CreateSession(_));
                    let session_create = matches!(action.as_ref(), ui::Action::CreateSession(_));
                    let reusable = if composer_connect || session_create {
                        match action.as_ref() {
                            ui::Action::Connect(connection)
                            | ui::Action::CreateSession(connection) => {
                                let key = connection.server_key();
                                self.tabs
                                    .iter()
                                    .find(|tab| {
                                        tab.server.as_ref() == Some(&key)
                                            && live_phase(tab.client.phase())
                                    })
                                    .map(|tab| sync::Arc::clone(&tab.client))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let promote = composer_connect || (composer_create && reusable.is_some());
                    {
                        if promote {
                            self.promote_composer()?;
                            promoted_composer = true;
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
                                let key = connection.server_key();
                                let started = if let Some(client) = reusable {
                                    client.ensure_policy(connection.clone())?;
                                    tab.client = client;
                                    Ok(())
                                } else if tab.server.is_some() {
                                    tab.client.reconnect(connection)
                                } else {
                                    tab.client.connect(connection)
                                }
                                .map(|()| {
                                    tab.server = Some(key);
                                    tab.label = label(&tab.ui.saved());
                                    tab.ui.reset_client_size();
                                    tab.ui.open_terminal();
                                });
                                // Remember where a successful connection pointed, so
                                // the next start reopens the same form.
                                save = started.is_ok();
                                return_to_composer = promoted_composer && started.is_err();
                                started
                            }
                            ui::Action::ListSessions(connection) => {
                                tab.client.list_sessions(connection)
                            }
                            ui::Action::MigrateSession(connection, source) => {
                                tab.client.migrate_session(connection, source)
                            }
                            ui::Action::TerminateSession(connection, source) => {
                                tab.client.terminate_session(connection, source)
                            }
                            ui::Action::CancelDiscovery => {
                                tab.client.cancel_discovery();
                                Ok(())
                            }
                            ui::Action::CreateSession(connection) => {
                                if let Some(client) = reusable {
                                    let key = connection.server_key();
                                    let name = connection.window.clone();
                                    let mut policy = connection;
                                    policy.access = crate::session::Access::Interactive;
                                    let started = client
                                        .ensure_policy(policy)
                                        .and_then(|()| client.create_window(name))
                                        .map(|()| {
                                            tab.client = client;
                                            tab.server = Some(key);
                                            tab.label = label(&tab.ui.saved());
                                            tab.ui.reset_client_size();
                                            tab.ui.open_terminal();
                                        });
                                    save = started.is_ok();
                                    return_to_composer = promoted_composer && started.is_err();
                                    started
                                } else {
                                    // The first window deliberately starts the managed
                                    // session through a bounded one-shot connection.
                                    tab.client
                                        .create_session(connection, crate::core::Size::default())
                                }
                            }
                            ui::Action::RenameSession(name) => {
                                self.submitted_rename = None;
                                anyhow::ensure!(
                                    tab.ui.saved().interactive,
                                    "read-only sessions cannot be renamed"
                                );
                                let parsed = core::SessionName::new(name)?;
                                let previous = tab.ui.session_name().to_owned();
                                if parsed.as_str() == previous {
                                    Ok(())
                                } else {
                                    let window = tab.ui.current_window().ok_or_else(|| {
                                        anyhow::anyhow!("logical session window is unavailable")
                                    })?;
                                    tab.client.rename_window(window, parsed, previous)
                                }
                            }
                            ui::Action::SetWindowOption {
                                window,
                                previous,
                                option,
                            } => tab.client.set_window_option(window, previous, option),
                            ui::Action::DeleteWindowOption { window, name } => {
                                tab.client.delete_window_option(window, name)
                            }
                            ui::Action::Disconnect => {
                                // Exit is the only way a session tab is removed.
                                close_after_exit = Some(id);
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
                                anyhow::ensure!(
                                    can_add_session
                                        || !actions.iter().any(|(_, action)| matches!(
                                            action,
                                            crate::input::Action::MoveToNewSession
                                        )),
                                    "at most {MAX_TABS} session tabs may be open"
                                );
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
                        self.arm_echo();
                    }
                    if save {
                        self.persist();
                    }
                    if return_to_composer
                        && let Some(index) = self.tabs.iter().position(|tab| tab.id == id)
                    {
                        self.return_tab_to_composer(index);
                        self.persist();
                    }
                    if let Some(id) = close_after_exit
                        && let Some(index) = self.tabs.iter().position(|tab| tab.id == id)
                    {
                        self.remove_tab(index);
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
    fn enter_confirms_rename_even_when_the_field_also_loses_focus() {
        assert_eq!(rename_key_outcome(true, false, true), RenameKey::Submit);
        assert_eq!(rename_key_outcome(true, false, false), RenameKey::Submit);
        assert_eq!(rename_key_outcome(false, true, true), RenameKey::Cancel);
        assert_eq!(rename_key_outcome(false, false, true), RenameKey::Cancel);
        assert_eq!(rename_key_outcome(false, false, false), RenameKey::Keep);
    }

    #[test]
    fn enter_in_the_real_tab_editor_submits_the_rename() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let id = workspace.tabs[0].id;
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let screen = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 760.0),
            )),
            ..Default::default()
        };
        // Let the terminal consume its one-time generation focus request before
        // the tab editor asks for focus, matching a real already-open tab.
        let _ = ctx.run_ui(screen(), |root| {
            workspace.show(root);
        });
        workspace.renaming = Some(SessionRename {
            id,
            draft: "Demo".into(),
            focus: true,
        });
        let _ = ctx.run_ui(screen(), |root| {
            workspace.show(root);
        });
        let _ = ctx.run_ui(screen(), |root| {
            workspace.show(root);
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Text("replacement".into())],
            ..screen()
        };
        let _ = ctx.run_ui(input, |root| {
            workspace.show(root);
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..screen()
        };
        let mut action = Action::None;
        let _ = ctx.run_ui(input, |root| {
            action = workspace.show(root);
        });
        assert!(
            matches!(&action, Action::Tab(tab, inner) if *tab == id && matches!(**inner, ui::Action::RenameSession(ref name) if name == "replacement")),
            "the first typing must replace the selected generated name; draft={:?}, submitted={:?}",
            workspace
                .renaming
                .as_ref()
                .map(|rename| rename.draft.as_str()),
            workspace.submitted_rename,
        );
    }

    #[test]
    fn a_submitted_rename_survives_a_second_show_pass() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let id = workspace.tabs[0].id;
        workspace.submitted_rename = Some((id, "renamed".into()));
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 760.0),
            )),
            ..Default::default()
        };
        let mut action = Action::None;
        let _ = ctx.run_ui(input.clone(), |root| {
            action = workspace.show(root);
        });
        assert!(
            matches!(action, Action::Tab(tab, ref inner) if tab == id && matches!(**inner, ui::Action::RenameSession(ref name) if name == "renamed")),
            "first pass keeps the submitted rename"
        );
        let mut action = Action::None;
        let _ = ctx.run_ui(input, |root| {
            action = workspace.show(root);
        });
        assert!(
            matches!(action, Action::Tab(tab, ref inner) if tab == id && matches!(**inner, ui::Action::RenameSession(ref name) if name == "renamed")),
            "a later layout pass must not drop the rename for terminal Enter"
        );
    }

    #[test]
    fn a_quiet_connected_tab_turns_blue() {
        let idle = egui::Color32::from_rgb(40, 40, 40);
        let live = tab_color(desktop::Phase::Watching, idle, false);
        let quiet = tab_color(desktop::Phase::Watching, idle, true);
        assert_ne!(live, quiet);
        assert_ne!(
            tab_color(desktop::Phase::Failed, idle, false),
            tab_color(desktop::Phase::Reconnecting, idle, false),
            "a broken tab is red, not the reconnecting yellow"
        );
        assert_eq!(
            tab_color(desktop::Phase::Failed, idle, false),
            tab_color(desktop::Phase::Disconnected, idle, false),
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
    fn exit_removes_every_registered_tab() {
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
        assert!(workspace.tabs.is_empty());
        assert!(workspace.composer_open);
    }

    #[test]
    fn a_failed_empty_tab_stays_registered() {
        let workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let failed_id = workspace.tabs[0].id;
        {
            let mut state = workspace.tabs[0].client.lock();
            state.view = None;
            state.phase = desktop::Phase::Failed;
            state.error = Some("authentication failed".into());
        }

        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.tabs[0].id, failed_id);
        assert!(!workspace.composer_open);
    }

    #[test]
    fn tab_overflow_range_keeps_the_active_tab_visible() {
        let widths = [60.0, 80.0, 70.0, 90.0, 50.0];
        let range = visible_tab_range(&widths, 3, 175.0);
        assert!(range.contains(&3));
        assert!(range.start > 0);
        assert!(range.end < widths.len());
        assert!(widths[range.clone()].iter().sum::<f32>() <= 175.0);
    }

    #[test]
    fn long_tab_names_are_compact_without_splitting_unicode() {
        let label = "abcdefghijklmnopqrstuvw界yz";
        let title = compact_tab_title(label);
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), 25);
    }

    #[test]
    fn a_background_failed_tab_is_not_closed_from_under_you() {
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        workspace.push_idle_tab().unwrap();
        workspace.active = 0;
        workspace.composer_open = false;
        let background = workspace.tabs[1].id;
        {
            let mut state = workspace.tabs[1].client.lock();
            state.view = None;
            state.phase = desktop::Phase::Failed;
            state.error = Some("The remote tmux server exited.".into());
        }

        assert!(
            workspace.tabs.iter().any(|tab| tab.id == background),
            "a failed background tab stays so it is not deleted from saved state"
        );
        assert!(workspace.notice.is_none());
        assert!(!workspace.composer_open);
    }

    #[test]
    fn tabs_are_labelled_by_session_only() {
        assert_eq!(
            label(&store::Tab {
                host: "build.example.test".into(),
                session: "ci".into(),
                ..store::Tab::default()
            }),
            "ci"
        );
        assert_eq!(
            label(&store::Tab {
                destination: "dev".into(),
                host: "10.0.0.2".into(),
                session: "work".into(),
                ..store::Tab::default()
            }),
            "work"
        );
        assert_eq!(label(&store::Tab::default()), NEW_CONNECTION);
    }

    #[test]
    fn endpoint_identity_ignores_alias_but_not_transport_details() {
        let left = store::Tab {
            destination: "dev".into(),
            host: "10.0.0.2".into(),
            user: "alice".into(),
            port: 22,
            socket: "/tmp/tmux.sock".into(),
            ..store::Tab::default()
        };
        let mut right = left.clone();
        right.destination = "another-alias".into();
        assert!(same_endpoint(&left, &right));
        right.port = 2222;
        assert!(!same_endpoint(&left, &right));
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
                            connection.window.as_str().to_owned(),
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
                ("10.0.0.2".into(), "starcom".into(), "work".into()),
                ("build.example.test".into(), "starcom".into(), "ci".into())
            ]
        );
        // The injected connector keeps this test off the network. Opening the
        // terminal here proves production startup will show connection progress
        // and then the restored tmux view, not the server chooser.
        for tab in &workspace.tabs {
            assert!(!tab.ui.showing_form());
        }
        assert_eq!(workspace.tabs[0].label, "work");
        assert_eq!(workspace.tabs[1].label, "ci");
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
                            connection.window.as_str().to_owned(),
                        ));
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert_eq!(started.tabs.len(), 1);
        assert_eq!(started.tabs[0].ui.saved().window, Some(0));
        assert_eq!(started.tabs[0].ui.saved().pane, Some(1));
        assert_eq!(started.tabs[0].label, "work");
        assert_eq!(
            resumed,
            Some(("dev.example.test".into(), "starcom".into(), "work".into()))
        );
        assert!(!started.tabs[0].ui.showing_form());
        assert!(!started.composer_open);
    }

    #[test]
    fn legacy_saves_do_not_preselect_or_start_a_client() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-workspace-legacy-{}-{:?}",
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
                    host: "zork.example.test".into(),
                    user: "alice".into(),
                    session: "zork/0".into(),
                    legacy: true,
                    known_hosts: "/tmp/known_hosts".into(),
                    history: store::DEFAULT_HISTORY,
                    interactive: true,
                    reconnect: true,
                    ..store::Tab::default()
                }],
                ..store::Workspace::default()
            },
        )
        .unwrap();

        let mut workspace = idle_workspace(Some(file.clone()));
        let mut resumed = 0;
        workspace
            .restore_with(
                |_, _| dialog::BrokenStore::Exit,
                |_, _| {
                    resumed += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(resumed, 0, "a legacy attachment started a runtime client");
        assert!(workspace.tabs.is_empty());
        assert!(workspace.composer_open);
        assert!(workspace.composer.ui.form.destination().is_empty());
        assert!(workspace.composer.ui.session_name().is_empty());

        workspace.persist();
        let saved = store::load(&file).unwrap().unwrap();
        assert!(saved.tabs.is_empty());
    }

    #[test]
    fn an_incomplete_active_saved_tab_moves_to_the_composer() {
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
        assert!(workspace.tabs.is_empty());
        assert!(workspace.composer_open);
        assert!(workspace.composer.ui.showing_form());
        assert!(
            workspace
                .composer
                .client
                .error()
                .is_some_and(|error| error.contains("choose a session"))
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
            submitted_rename: None,
        }
    }

    #[test]
    fn restored_windows_on_one_server_share_one_client_and_resume() {
        let directory = std::env::temp_dir().join(format!(
            "starcom-shared-{}-{:?}",
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
        let tab = |session: &str| store::Tab {
            host: "zork.example.test".into(),
            user: "alice".into(),
            session: session.into(),
            known_hosts: "/tmp/known_hosts".into(),
            port: 22,
            history: store::DEFAULT_HISTORY,
            interactive: true,
            reconnect: true,
            ..store::Tab::default()
        };
        store::save(
            &file,
            &store::Workspace {
                tabs: vec![tab("work"), tab("build")],
                ..store::Workspace::default()
            },
        )
        .unwrap();

        let mut workspace = idle_workspace(Some(file));
        let mut resumed = Vec::new();
        workspace
            .restore_with(
                |_, _| dialog::BrokenStore::Exit,
                |_, connection| {
                    resumed.push(connection.window.as_str().to_owned());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(resumed, ["work"]);
        assert_eq!(workspace.tabs.len(), 2);
        assert!(sync::Arc::ptr_eq(
            &workspace.tabs[0].client,
            &workspace.tabs[1].client
        ));
        assert_eq!(workspace.tabs[0].server, workspace.tabs[1].server);
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
    fn overflowing_tabs_keep_the_top_panel_to_one_row() {
        let ctx = egui::Context::default();
        crate::window::configure(&ctx);
        let mut workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        for _ in 1..MAX_TABS {
            workspace.push_idle_tab().unwrap();
        }
        for pass in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 480.0),
                )),
                time: Some(pass as f64 / 60.0),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |root| {
                workspace.show(root);
            });
        }
        let tabs_h =
            egui::containers::panel::PanelState::load(&ctx, egui::Id::new("connection-tabs"))
                .map(|state| state.rect.height())
                .unwrap_or(0.0);
        assert!(
            (1.0..48.0).contains(&tabs_h),
            "tab overflow must stay on one row, height was {tabs_h}"
        );
    }

    #[test]
    fn sending_input_raises_the_remote_paint_rate() {
        let workspace = Workspace::new(sync::Arc::new(|| {}), desktop::Startup::Demo).unwrap();
        let idle = workspace.repaint_interval();
        assert_eq!(workspace.paint_interval(), idle);
        let mut workspace = workspace;
        workspace.arm_echo();
        assert_eq!(
            workspace.paint_interval(),
            idle.min(time::Duration::from_millis(50))
        );
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
