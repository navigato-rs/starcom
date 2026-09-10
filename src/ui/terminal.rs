//! Visible-row terminal painting and local selection. No TextEdit, no widget per
//! cell, and no path from GUI input to the remote application in this increment.

use alacritty_terminal::{grid::Dimensions, index, selection, term, vte::ansi};

use crate::{input, snapshot};

use super::layout;

const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(18, 21, 26);
const FOREGROUND: egui::Color32 = egui::Color32::from_rgb(214, 220, 229);

/// Keyboard-focus id for a pane. The white border and key translation both
/// use this; they must stay the same widget egui's focus graph knows about.
pub(crate) fn focus_id(generation: u64, pane: tmuxctl::PaneId) -> egui::Id {
    egui::Id::new(("terminal", generation, pane.0))
}

/// Font cell size used both to paint and to tell tmux the client size.
pub(crate) fn cell_metrics(ui: &mut egui::Ui, font_size: f32) -> (f32, f32) {
    let font = egui::FontId::monospace(font_size);
    ui.fonts_mut(|fonts| {
        (
            fonts.glyph_width(&font, 'M'),
            fonts.row_height(&font).ceil() + 2.0,
        )
    })
}

pub struct PaneUi {
    pub rect: egui::Rect,
    pub painted_rows: usize,
    /// Points of leftover wheel (egui smoothing after a notch) not yet a tick.
    remainder: f32,
    /// Sub-row remainder so a trackpad does not snap to whole history lines.
    scroll_frac: f32,
    /// True while the viewport is pinned to the live tip.
    stuck: bool,
}

impl Default for PaneUi {
    fn default() -> Self {
        Self {
            rect: egui::Rect::NOTHING,
            painted_rows: 0,
            remainder: 0.0,
            scroll_frac: 0.0,
            stuck: true,
        }
    }
}

/// One application-wheel tick. Smaller than a local history line so the first
/// trackpad fragment is not held until 40 points have piled up.
const WHEEL_LINE: f32 = 20.0;

fn at_live_tip(scroll_y: f32, content_height: f32, view_height: f32) -> bool {
    let max_off = (content_height - view_height).max(0.0);
    max_off <= 1.0 || scroll_y >= max_off - 1.0
}

/// Pixel offset of a history viewport. `display_offset` 0 is the live tip.
fn scroll_origin(history: usize, display_offset: usize, row_height: f32, frac: f32) -> f32 {
    history.saturating_sub(display_offset) as f32 * row_height + frac
}

/// Offset that shows the last `view_height` of the buffer. The content height
/// itself is past the last row; show_rows uses the offset before egui clamps,
/// so that paints into empty space (blank panes with a sliver of the prompt).
fn tip_origin(total_rows: usize, row_height: f32, view_height: f32) -> f32 {
    (total_rows as f32 * row_height - view_height).max(0.0)
}

struct HistoryViewport {
    stuck: bool,
    frac: f32,
    offset: usize,
}

fn history_viewport(
    scroll_y: f32,
    content_height: f32,
    view_height: f32,
    row_height: f32,
    history: usize,
    was_stuck: bool,
) -> HistoryViewport {
    if at_live_tip(scroll_y, content_height, view_height) {
        return HistoryViewport {
            stuck: true,
            frac: 0.0,
            offset: 0,
        };
    }
    // A stuck pane whose offset jumped to 0 is a lost ScrollArea state, not a
    // user scroll to the oldest line. Treating it as the latter locks the view
    // on blank history after a reconnect (empty frozen panes).
    if was_stuck && scroll_y <= 1.0 {
        return HistoryViewport {
            stuck: true,
            frac: 0.0,
            offset: 0,
        };
    }
    let whole = (scroll_y / row_height).floor() as usize;
    HistoryViewport {
        stuck: false,
        frac: (scroll_y - whole as f32 * row_height).clamp(0.0, row_height),
        offset: history.saturating_sub(whole),
    }
}

fn screen_cell(
    position: egui::Pos2,
    content: egui::Rect,
    cell_width: f32,
    row_height: f32,
    columns: usize,
    rows: usize,
) -> (usize, usize) {
    let column = ((position.x - content.left()) / cell_width)
        .floor()
        .clamp(0.0, columns.saturating_sub(1) as f32) as usize;
    let row = ((position.y - content.top()) / row_height)
        .floor()
        .clamp(0.0, rows.saturating_sub(1) as f32) as usize;
    (column, row)
}

fn wheel_ticks(remainder: &mut f32, delta: f32) -> i32 {
    *remainder += delta;
    let ticks = (*remainder / WHEEL_LINE) as i32;
    let ticks = ticks.clamp(-8, 8);
    *remainder -= ticks as f32 * WHEEL_LINE;
    ticks
}

impl PaneUi {
    /// A snapshot replace rebuilds the Alacritty model at offset 0. If the
    /// previous model was scrolled, stay unstuck so the copied offset paints.
    #[cfg(test)]
    pub(crate) fn keep_history_viewport(&mut self, offset: usize) {
        if offset > 0 {
            self.stuck = false;
        }
    }

    /// Layout rebuilds keep stuck/offset; only the sub-row remainder is stale
    /// because row height and pane size have changed.
    pub(crate) fn on_layout_rebuild(&mut self) {
        self.scroll_frac = 0.0;
    }

    /// Keyboard input and paste snap the viewport to the live tip, like a
    /// conventional terminal. The wheel does not.
    pub(crate) fn follow_live_tip(&mut self) {
        self.stuck = true;
        self.scroll_frac = 0.0;
    }

    #[cfg(test)]
    pub(crate) fn is_stuck(&self) -> bool {
        self.stuck
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        pane: &mut snapshot::Pane,
        generation: u64,
        font_size: f32,
        focused: &mut Option<tmuxctl::PaneId>,
        notice: &mut Option<String>,
        notice_until: &mut Option<std::time::Instant>,
        controls: bool,
        can_kill: bool,
        neighbors: layout::Neighbors,
        zoomed: bool,
        frozen: bool,
    ) -> Vec<input::Action> {
        self.rect = rect;
        self.painted_rows = 0;
        let pane_id = pane.state.pane;
        let id = focus_id(generation, pane_id);
        if rect.width() < 16.0 || rect.height() < 40.0 {
            if *focused == Some(pane_id) {
                *focused = None;
            }
            return Vec::new();
        }
        let mut events = Vec::new();
        // The white border is keyboard focus, not a sticky selection.
        let active = ui.ctx().memory(|memory| memory.has_focus(id));
        if active {
            *focused = Some(pane_id);
        } else if *focused == Some(pane_id) {
            *focused = None;
        }
        let border = if active {
            ui.visuals().selection.stroke.color
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        ui.painter().rect_filled(rect, 3.0, BACKGROUND);
        ui.painter().rect_stroke(
            rect,
            3.0,
            egui::Stroke::new(1.0_f32, border),
            egui::StrokeKind::Inside,
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("pane-scope", pane_id.0))
                .max_rect(rect.shrink(4.0)),
            |ui| {
                ui.set_clip_rect(rect.shrink(1.0).intersect(ui.clip_rect()));
                ui.set_min_size(rect.shrink(4.0).size());
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                // Paint tmux's grid at the real font cell size. Never locally
                // reflow it: Alacritty resize is not tmux resize, and that is
                // what scrambled shells after a font-size or window change.
                let columns = pane.terminal.size().columns();
                let screen_rows = pane.terminal.size().rows();
                let font = egui::FontId::monospace(font_size);
                let (cell_width, row_height) = cell_metrics(ui, font_size);
                let history = pane.terminal.model().grid().history_size();
                let total_rows = pane.terminal.model().grid().total_lines();
                let display = pane.terminal.history_offset();
                let rows = &mut self.painted_rows;
                let remainder = &mut self.remainder;
                let mouse = pane.terminal.reports_mouse();
                let wants_wheel = pane.terminal.wants_wheel();
                let sgr_mouse = pane.terminal.sgr_mouse();
                // Follow the live tip with content-minus-view, not the content
                // height: show_rows applies the offset before clamp, and an
                // offset at the content height sits past the last row. Once
                // the user has scrolled up, Alacritty's display_offset is
                // the lock. Default ScrollArea offset is 0 (oldest history).
                let mut area = egui::ScrollArea::vertical()
                    .id_salt(("starcom-scroll", pane_id.0))
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.stuck)
                    .scroll_source(if wants_wheel {
                        egui::scroll_area::ScrollSource::NONE
                    } else {
                        // Wheel and the scrollbar scroll. Dragging the
                        // contents is local selection, not a pan.
                        egui::scroll_area::ScrollSource::SCROLL_BAR
                            | egui::scroll_area::ScrollSource::MOUSE_WHEEL
                    });
                let tip = tip_origin(total_rows, row_height, ui.available_height());
                if self.stuck {
                    area = area.vertical_scroll_offset(tip);
                } else {
                    area = area.vertical_scroll_offset(
                        scroll_origin(history, display, row_height, self.scroll_frac).min(tip),
                    );
                }
                let output = area.show_rows(ui, row_height, total_rows, |ui, range| {
                    *rows = range.len();
                    let (_, content) = ui.allocate_space(egui::vec2(
                        columns as f32 * cell_width,
                        range.len() as f32 * row_height,
                    ));
                    let response = ui
                        .interact(content, id, egui::Sense::click_and_drag())
                        .on_hover_cursor(egui::CursorIcon::Text);
                    let point_at = |position: egui::Pos2| {
                        let row = (range.start as isize
                            + ((position.y - content.top()) / row_height).floor() as isize)
                            .clamp(0, total_rows as isize - 1)
                            as usize;
                        let x = ((position.x - content.left()) / cell_width)
                            .clamp(0.0, columns as f32 - 0.001);
                        (
                            index::Point::new(
                                index::Line(row as i32 - history as i32),
                                index::Column(x as usize),
                            ),
                            if x.fract() < 0.5 {
                                index::Side::Left
                            } else {
                                index::Side::Right
                            },
                        )
                    };
                    if response.clicked() || response.drag_started() || response.secondary_clicked()
                    {
                        *focused = Some(pane_id);
                        response.request_focus();
                    }
                    if response.has_focus() {
                        // Last-frame filter is what begin_pass consults, so
                        // this has to be set every frame we hold focus.
                        // Otherwise ArrowUp walks to Paste and Tab/Escape
                        // leave the remote application.
                        ui.ctx().memory_mut(|memory| {
                            memory.set_focus_lock_filter(
                                id,
                                egui::EventFilter {
                                    tab: true,
                                    horizontal_arrows: true,
                                    vertical_arrows: true,
                                    escape: true,
                                },
                            );
                        });
                    }
                    if response.clicked() {
                        events.push(input::Action::SelectPane);
                    }
                    let pointer_in_pane = ui.rect_contains_pointer(rect);
                    if wants_wheel && pointer_in_pane {
                        let dy = ui.input(|input| input.smooth_scroll_delta.y);
                        let ticks = wheel_ticks(remainder, dy);
                        if ticks != 0 {
                            let up = ticks > 0;
                            let n = ticks.unsigned_abs();
                            // Mouse-wheel SGR reports on the primary screen are
                            // what made Grok (and similar TUIs) treat a scroll
                            // as a selection. Alternate-screen mouse apps such
                            // as vim still get the mouse protocol.
                            if mouse && pane.terminal.is_alternate_screen() {
                                let (column, row) = response
                                    .hover_pos()
                                    .map(|position| {
                                        screen_cell(
                                            position,
                                            content,
                                            cell_width,
                                            row_height,
                                            columns,
                                            screen_rows,
                                        )
                                    })
                                    .unwrap_or((0, 0));
                                for _ in 0..n {
                                    events.push(input::Action::Bytes(input::mouse_wheel_bytes(
                                        up, column, row, sgr_mouse,
                                    )));
                                }
                            } else {
                                let key = if up {
                                    input::Key::WheelUp
                                } else {
                                    input::Key::WheelDown
                                };
                                for _ in 0..n {
                                    events
                                        .push(input::Action::Key(key, input::Modifiers::default()));
                                }
                            }
                        }
                        // Remainder after the ±8 clamp is still a real scroll.
                        // Paint again so it is not held until the next notch.
                        if remainder.abs() >= WHEEL_LINE {
                            ui.ctx().request_repaint();
                        }
                        ui.ctx().input_mut(|input| {
                            input.smooth_scroll_delta.y = 0.0;
                        });
                    } else if !pointer_in_pane {
                        *remainder = 0.0;
                    }
                    if let Some(position) = response.interact_pointer_pos() {
                        let (point, side) = point_at(position);
                        let unmodified = ui.input(|input| {
                            let modifiers = input.modifiers;
                            !modifiers.shift
                                && !modifiers.ctrl
                                && !modifiers.alt
                                && !modifiers.command
                        });
                        // Unmodified single clicks belong to the application
                        // when it asked for them. Drags, modified clicks, and
                        // double/triple clicks stay local selection.
                        let forward_click = !frozen
                            && mouse
                            && unmodified
                            && response.clicked()
                            && !response.double_clicked()
                            && !response.triple_clicked();
                        if forward_click {
                            let (column, row) = screen_cell(
                                position,
                                content,
                                cell_width,
                                row_height,
                                columns,
                                screen_rows,
                            );
                            events.push(input::Action::Bytes(input::mouse_click_bytes(
                                true, column, row, sgr_mouse,
                            )));
                            events.push(input::Action::Bytes(input::mouse_click_bytes(
                                false, column, row, sgr_mouse,
                            )));
                            pane.terminal.clear_selection();
                        } else if response.triple_clicked() {
                            pane.terminal.begin_selection(
                                point,
                                side,
                                selection::SelectionType::Lines,
                            );
                            if let Some(text) = pane.terminal.selected_text() {
                                copy(ui.ctx(), text, notice, notice_until, "Copied!");
                                pane.terminal.clear_selection();
                            }
                        } else if response.double_clicked() {
                            pane.terminal.begin_selection(
                                point,
                                side,
                                selection::SelectionType::Semantic,
                            );
                            if let Some(text) = pane.terminal.selected_text() {
                                copy(ui.ctx(), text, notice, notice_until, "Copied!");
                                pane.terminal.clear_selection();
                            }
                        } else if response.clicked() {
                            pane.terminal.clear_selection();
                        } else if response.drag_started() {
                            let origin = ui
                                .input(|input| input.pointer.press_origin())
                                .unwrap_or(position);
                            let (anchor, anchor_side) = point_at(origin);
                            pane.terminal.begin_selection(
                                anchor,
                                anchor_side,
                                selection::SelectionType::Simple,
                            );
                        }
                        if response.dragged() {
                            pane.terminal.update_selection(point, side);
                            let clip = ui.clip_rect();
                            let delta = if position.y < clip.top() + 12.0 {
                                row_height
                            } else if position.y > clip.bottom() - 12.0 {
                                -row_height
                            } else {
                                0.0
                            };
                            if delta != 0.0 {
                                ui.scroll_with_delta(egui::vec2(0.0, delta));
                                ui.ctx()
                                    .request_repaint_after(std::time::Duration::from_millis(30));
                            }
                        }
                    }
                    if response.drag_stopped()
                        && let Some(text) = pane.terminal.selected_text()
                    {
                        copy(ui.ctx(), text, notice, notice_until, "Copied!");
                        pane.terminal.clear_selection();
                    }
                    let model = pane.terminal.model();
                    let selection_range = pane.terminal.selection_range();
                    let grid = model.grid();
                    let clip = ui.clip_rect();
                    for row in range.clone() {
                        let line = index::Line(row as i32 - history as i32);
                        let y = content.top() + (row - range.start) as f32 * row_height;
                        // Paint backgrounds before glyphs. In particular, a wide
                        // glyph spans its following spacer cell; painting that
                        // spacer afterwards would erase half of the glyph.
                        for column in 0..columns {
                            let cell = &grid[line][index::Column(column)];
                            let point = index::Point::new(line, index::Column(column));
                            let selected = selection_range.is_some_and(|range| {
                                range.contains(point)
                                    || cell.flags.contains(term::cell::Flags::WIDE_CHAR)
                                        && range.contains(index::Point::new(
                                            point.line,
                                            point.column + 1,
                                        ))
                            });
                            let background = if selected {
                                ui.visuals().selection.bg_fill
                            } else {
                                let background = cell_colors(cell, model.colors()).1;
                                if frozen {
                                    freeze(background)
                                } else {
                                    background
                                }
                            };
                            if background != BACKGROUND {
                                let cell_rect = egui::Rect::from_min_size(
                                    egui::pos2(content.left() + column as f32 * cell_width, y),
                                    egui::vec2(cell_width, row_height),
                                );
                                ui.painter().rect_filled(cell_rect, 0.0, background);
                            }
                        }
                        let mut column = 0;
                        while column < columns {
                            let cell = &grid[line][index::Column(column)];
                            let foreground = {
                                let color = cell_colors(cell, model.colors()).0;
                                if frozen { freeze(color) } else { color }
                            };
                            if cell.flags.intersects(
                                term::cell::Flags::WIDE_CHAR_SPACER
                                    | term::cell::Flags::LEADING_WIDE_CHAR_SPACER
                                    | term::cell::Flags::HIDDEN,
                            ) {
                                column += 1;
                                continue;
                            }
                            // Batch ordinary ASCII into fixed-width runs. A wide or
                            // combining glyph gets its own positioned cell cluster,
                            // so fallback font metrics cannot move the next column.
                            let start = column;
                            let mut text = String::new();
                            if cell.c.is_ascii() && cell.zerowidth().is_none_or(<[char]>::is_empty)
                            {
                                while column < columns {
                                    let next = &grid[line][index::Column(column)];
                                    if !next.c.is_ascii()
                                        || next.zerowidth().is_some_and(|chars| !chars.is_empty())
                                        || next.flags != cell.flags
                                        || next.fg != cell.fg
                                        || next.bg != cell.bg
                                    {
                                        break;
                                    }
                                    text.push(if next.c.is_control() { ' ' } else { next.c });
                                    column += 1;
                                }
                            } else {
                                text.push(cell.c);
                                if let Some(chars) = cell.zerowidth() {
                                    text.extend(chars);
                                }
                                column += 1;
                            }
                            if text.trim().is_empty()
                                && !cell.flags.intersects(
                                    term::cell::Flags::ALL_UNDERLINES
                                        | term::cell::Flags::STRIKEOUT,
                                )
                            {
                                continue;
                            }
                            let format = egui::TextFormat {
                                font_id: font.clone(),
                                color: foreground,
                                italics: cell.flags.contains(term::cell::Flags::ITALIC),
                                underline: if cell
                                    .flags
                                    .intersects(term::cell::Flags::ALL_UNDERLINES)
                                {
                                    egui::Stroke::new(1.0_f32, foreground)
                                } else {
                                    egui::Stroke::NONE
                                },
                                strikethrough: if cell.flags.contains(term::cell::Flags::STRIKEOUT)
                                {
                                    egui::Stroke::new(1.0_f32, foreground)
                                } else {
                                    egui::Stroke::NONE
                                },
                                ..Default::default()
                            };
                            let job = egui::text::LayoutJob::simple_format(text, format);
                            let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                            let x = content.left() + start as f32 * cell_width;
                            let width = if cell.flags.contains(term::cell::Flags::WIDE_CHAR) {
                                2
                            } else {
                                column - start
                            };
                            let glyph_clip = egui::Rect::from_min_size(
                                egui::pos2(x, y),
                                egui::vec2(width as f32 * cell_width, row_height),
                            );
                            ui.painter()
                                .with_clip_rect(clip.intersect(glyph_clip))
                                .galley(
                                    egui::pos2(x, y + (row_height - galley.size().y) * 0.5),
                                    galley,
                                    foreground,
                                );
                        }
                    }
                    let cursor = model.renderable_content().cursor;
                    let row = (cursor.point.line.0 + history as i32) as usize;
                    if !frozen && range.contains(&row) && cursor.shape != ansi::CursorShape::Hidden
                    {
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(
                                content.left() + cursor.point.column.0 as f32 * cell_width,
                                content.top() + (row - range.start) as f32 * row_height,
                            ),
                            egui::vec2(cell_width, row_height),
                        );
                        let stroke = egui::Stroke::new(1.0_f32, FOREGROUND);
                        match cursor.shape {
                            ansi::CursorShape::Beam => {
                                ui.painter()
                                    .line_segment([rect.left_top(), rect.left_bottom()], stroke);
                            }
                            ansi::CursorShape::Underline => {
                                ui.painter().line_segment(
                                    [rect.left_bottom(), rect.right_bottom()],
                                    stroke,
                                );
                            }
                            _ if active => {
                                ui.painter().rect_filled(rect.shrink(0.5), 0.0, FOREGROUND);
                                let cell = &grid[cursor.point.line][cursor.point.column];
                                if !cell.c.is_control() && cell.c != ' ' {
                                    let mut text = String::from(cell.c);
                                    if let Some(chars) = cell.zerowidth() {
                                        text.extend(chars);
                                    }
                                    let format = egui::TextFormat {
                                        font_id: font.clone(),
                                        color: BACKGROUND,
                                        ..Default::default()
                                    };
                                    let job = egui::text::LayoutJob::simple_format(text, format);
                                    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                                    ui.painter().galley(
                                        egui::pos2(
                                            rect.left(),
                                            rect.top() + (row_height - galley.size().y) * 0.5,
                                        ),
                                        galley,
                                        BACKGROUND,
                                    );
                                }
                            }
                            _ => {
                                ui.painter().rect_stroke(
                                    rect.shrink(0.5),
                                    0.0,
                                    stroke,
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                });
                let viewport = history_viewport(
                    output.state.offset.y,
                    output.content_size.y,
                    output.inner_rect.height(),
                    row_height,
                    history,
                    self.stuck,
                );
                self.stuck = viewport.stuck;
                self.scroll_frac = viewport.frac;
                pane.terminal.scroll_history(viewport.offset);
                if controls && *focused == Some(pane_id) {
                    let buttons = 3 + usize::from(can_kill) + neighbors.count();
                    let width = 8.0 + buttons as f32 * 24.0;
                    let bar = egui::Rect::from_min_max(
                        egui::pos2(rect.max.x - width - 4.0, rect.min.y + 4.0),
                        egui::pos2(rect.max.x - 4.0, rect.min.y + 28.0),
                    )
                    .intersect(rect);
                    egui::Area::new(id.with("chrome"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(bar.min)
                        .show(ui.ctx(), |ui| {
                            ui.set_max_size(bar.size());
                            egui::Frame::NONE
                                .fill(egui::Color32::from_rgba_unmultiplied(16, 18, 22, 220))
                                .corner_radius(5.0)
                                .inner_margin(egui::Margin::symmetric(4, 2))
                                .show(ui, |ui| {
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.spacing_mut().item_spacing.x = 2.0;
                                            if can_kill
                                                && chrome_button(
                                                    ui,
                                                    ChromeIcon::Close,
                                                    "Close this pane",
                                                )
                                            {
                                                events.push(input::Action::KillPane);
                                            }
                                            let (zoom_icon, zoom_tip) = if zoomed {
                                                (ChromeIcon::Restore, "Restore pane layout")
                                            } else {
                                                (ChromeIcon::Zoom, "Maximize this pane")
                                            };
                                            if chrome_button(ui, zoom_icon, zoom_tip) {
                                                events.push(input::Action::ZoomPane);
                                                ui.ctx()
                                                    .memory_mut(|memory| memory.request_focus(id));
                                            }
                                            for (icon, tip, other) in [
                                                (ChromeIcon::MoveDown, "Move down", neighbors.down),
                                                (ChromeIcon::MoveUp, "Move up", neighbors.up),
                                                (
                                                    ChromeIcon::MoveRight,
                                                    "Move right",
                                                    neighbors.right,
                                                ),
                                                (ChromeIcon::MoveLeft, "Move left", neighbors.left),
                                            ] {
                                                if let Some(other) = other
                                                    && chrome_button(ui, icon, tip)
                                                {
                                                    events.push(input::Action::SwapPane(other));
                                                    ui.ctx().memory_mut(|memory| {
                                                        memory.request_focus(id)
                                                    });
                                                }
                                            }
                                            if chrome_button(
                                                ui,
                                                ChromeIcon::SplitBelow,
                                                "Split below",
                                            ) {
                                                events
                                                    .push(input::Action::Split(input::Axis::Rows));
                                                ui.ctx()
                                                    .memory_mut(|memory| memory.request_focus(id));
                                            }
                                            if chrome_button(
                                                ui,
                                                ChromeIcon::SplitRight,
                                                "Split right",
                                            ) {
                                                events.push(input::Action::Split(
                                                    input::Axis::Columns,
                                                ));
                                                ui.ctx()
                                                    .memory_mut(|memory| memory.request_focus(id));
                                            }
                                        },
                                    );
                                });
                        });
                }
            },
        );
        events
    }
}

#[derive(Clone, Copy)]
enum ChromeIcon {
    SplitRight,
    SplitBelow,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Zoom,
    Restore,
    Close,
}

fn chrome_button(ui: &mut egui::Ui, icon: ChromeIcon, tip: &str) -> bool {
    let id = ui.id().with(tip);
    let (_, rect) = ui.allocate_space(egui::vec2(22.0, 20.0));
    let response = ui.interact(rect, id, egui::Sense::CLICK);
    let hovered = response.hovered();
    let fill = if hovered {
        match icon {
            ChromeIcon::Close => egui::Color32::from_rgb(168, 52, 52),
            _ => egui::Color32::from_white_alpha(28),
        }
    } else {
        egui::Color32::TRANSPARENT
    };
    if fill.a() > 0 {
        ui.painter().rect_filled(rect, 4.0, fill);
    }
    let color = if hovered {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_gray(168)
    };
    paint_chrome_icon(
        ui.painter(),
        rect.shrink2(egui::vec2(5.0, 4.5)),
        icon,
        color,
    );
    response.on_hover_text(tip).clicked()
}

fn paint_chrome_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: ChromeIcon,
    color: egui::Color32,
) {
    let stroke = egui::Stroke::new(1.4_f32, color);
    match icon {
        ChromeIcon::SplitRight => {
            painter.rect_stroke(rect, 1.5, stroke, egui::StrokeKind::Outside);
            let x = rect.center().x;
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                stroke,
            );
        }
        ChromeIcon::SplitBelow => {
            painter.rect_stroke(rect, 1.5, stroke, egui::StrokeKind::Outside);
            let y = rect.center().y;
            painter.line_segment(
                [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                stroke,
            );
        }
        ChromeIcon::Zoom => {
            painter.rect_stroke(rect.shrink(0.5), 1.5, stroke, egui::StrokeKind::Outside);
        }
        ChromeIcon::Restore => {
            let back = egui::Rect::from_min_max(
                rect.min + egui::vec2(2.0, 0.0),
                rect.max - egui::vec2(0.0, 2.0),
            );
            let front = egui::Rect::from_min_max(
                rect.min + egui::vec2(0.0, 2.0),
                rect.max - egui::vec2(2.0, 0.0),
            );
            painter.rect_stroke(back, 1.0, stroke, egui::StrokeKind::Outside);
            painter.rect_filled(front, 1.0, BACKGROUND);
            painter.rect_stroke(front, 1.0, stroke, egui::StrokeKind::Outside);
        }
        ChromeIcon::Close => {
            let r = rect.shrink(1.0);
            painter.line_segment([r.left_top(), r.right_bottom()], stroke);
            painter.line_segment([r.right_top(), r.left_bottom()], stroke);
        }
        ChromeIcon::MoveLeft => {
            let c = rect.center();
            painter.line_segment(
                [egui::pos2(c.x + 3.0, c.y - 4.5), egui::pos2(c.x - 3.0, c.y)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x - 3.0, c.y), egui::pos2(c.x + 3.0, c.y + 4.5)],
                stroke,
            );
        }
        ChromeIcon::MoveRight => {
            let c = rect.center();
            painter.line_segment(
                [egui::pos2(c.x - 3.0, c.y - 4.5), egui::pos2(c.x + 3.0, c.y)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x + 3.0, c.y), egui::pos2(c.x - 3.0, c.y + 4.5)],
                stroke,
            );
        }
        ChromeIcon::MoveUp => {
            let c = rect.center();
            painter.line_segment(
                [egui::pos2(c.x - 4.5, c.y + 3.0), egui::pos2(c.x, c.y - 3.0)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y - 3.0), egui::pos2(c.x + 4.5, c.y + 3.0)],
                stroke,
            );
        }
        ChromeIcon::MoveDown => {
            let c = rect.center();
            painter.line_segment(
                [egui::pos2(c.x - 4.5, c.y - 3.0), egui::pos2(c.x, c.y + 3.0)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y + 3.0), egui::pos2(c.x + 4.5, c.y - 3.0)],
                stroke,
            );
        }
    }
}

pub fn copy(
    ctx: &egui::Context,
    text: String,
    notice: &mut Option<String>,
    notice_until: &mut Option<std::time::Instant>,
    message: &str,
) {
    if text.len() > 1024 * 1024 {
        *notice = Some("Selection exceeds the 1 MiB clipboard limit.".to_owned());
        *notice_until = None;
    } else {
        ctx.copy_text(text);
        *notice = Some(message.to_owned());
        *notice_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
    }
}

fn freeze(color: egui::Color32) -> egui::Color32 {
    let mix = |channel: u8, toward: u8| ((u16::from(channel) * 5 + u16::from(toward)) / 6) as u8;
    egui::Color32::from_rgb(
        mix(color.r(), BACKGROUND.r()),
        mix(color.g(), BACKGROUND.g()),
        mix(color.b(), BACKGROUND.b()),
    )
}

fn cell_colors(
    cell: &term::cell::Cell,
    colors: &term::color::Colors,
) -> (egui::Color32, egui::Color32) {
    let mut foreground = color(cell.fg, colors);
    let mut background = color(cell.bg, colors);
    if cell.flags.contains(term::cell::Flags::BOLD) {
        foreground = match cell.fg {
            ansi::Color::Named(named) if (named as usize) < 8 => {
                indexed(named as usize + 8, colors)
            }
            ansi::Color::Indexed(index) if index < 8 => indexed(usize::from(index) + 8, colors),
            _ => foreground,
        };
    }
    if cell.flags.contains(term::cell::Flags::DIM) {
        foreground = foreground.gamma_multiply(0.65);
    }
    if cell.flags.contains(term::cell::Flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    (foreground, background)
}

fn color(color: ansi::Color, colors: &term::color::Colors) -> egui::Color32 {
    match color {
        ansi::Color::Spec(rgb) => egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        ansi::Color::Indexed(index) => indexed(usize::from(index), colors),
        ansi::Color::Named(named) => indexed(named as usize, colors),
    }
}

fn indexed(index: usize, colors: &term::color::Colors) -> egui::Color32 {
    if let Some(rgb) = colors[index] {
        return egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b);
    }
    const ANSI: [[u8; 3]; 16] = [
        [30, 34, 42],
        [224, 108, 117],
        [152, 195, 121],
        [229, 192, 123],
        [97, 175, 239],
        [198, 120, 221],
        [86, 182, 194],
        [214, 220, 229],
        [110, 121, 138],
        [239, 134, 143],
        [174, 217, 143],
        [245, 215, 151],
        [129, 194, 249],
        [215, 151, 235],
        [121, 205, 213],
        [244, 246, 250],
    ];
    let rgb = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let n = index - 16;
            let level = |n| if n == 0 { 0 } else { 55 + n as u8 * 40 };
            [level(n / 36), level(n / 6 % 6), level(n % 6)]
        }
        232..=255 => [8 + (index - 232) as u8 * 10; 3],
        257 => return BACKGROUND,
        268 => return FOREGROUND.gamma_multiply(0.65),
        259..=266 => return indexed(index - 259, colors).gamma_multiply(0.65),
        _ => return FOREGROUND,
    };
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_rgb_and_inverse_colors_are_preserved() {
        let colors = term::color::Colors::default();
        assert_eq!(indexed(196, &colors), egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(
            color(ansi::Color::Named(ansi::NamedColor::DimForeground), &colors),
            FOREGROUND.gamma_multiply(0.65)
        );
        let cell = term::cell::Cell {
            fg: ansi::Color::Spec(ansi::Rgb {
                r: 12,
                g: 34,
                b: 56,
            }),
            bg: ansi::Color::Named(ansi::NamedColor::Background),
            flags: term::cell::Flags::INVERSE,
            ..Default::default()
        };
        assert_eq!(
            cell_colors(&cell, &colors),
            (BACKGROUND, egui::Color32::from_rgb(12, 34, 56))
        );
    }

    #[test]
    fn freeze_dims_toward_the_background() {
        let frozen = freeze(egui::Color32::from_rgb(255, 0, 0));
        assert!(frozen.r() > frozen.g(), "hue is kept, only dimmed");
        assert!(frozen.r() < 255);
        assert!(frozen.r() > BACKGROUND.r());
    }

    #[test]
    fn leftover_wheel_accumulates_instead_of_rounding_up() {
        let mut remainder = 0.0;
        assert_eq!(wheel_ticks(&mut remainder, 8.0), 0);
        assert_eq!(wheel_ticks(&mut remainder, 8.0), 0);
        assert_eq!(wheel_ticks(&mut remainder, 8.0), 1);
        assert!(remainder.abs() < 5.0, "{remainder}");
        remainder = 0.0;
        assert_eq!(wheel_ticks(&mut remainder, -20.0), -1);
        assert_eq!(remainder, 0.0);
        assert_eq!(wheel_ticks(&mut remainder, 80.0), 4);
    }

    #[test]
    fn tip_origin_is_the_last_viewport_not_past_the_end() {
        assert!((tip_origin(100, 20.0, 800.0) - 1200.0).abs() < 0.01);
        assert_eq!(tip_origin(10, 20.0, 800.0), 0.0);
    }

    #[test]
    fn at_live_tip_only_when_offset_is_at_the_end() {
        assert!(!at_live_tip(800.0, 2000.0, 800.0));
        assert!(at_live_tip(1200.0, 2000.0, 800.0));
        assert!(at_live_tip(0.0, 200.0, 800.0));
    }

    #[test]
    fn viewport_moves_up_the_list_when_history_is_full() {
        let row_height = 20.0;
        let origin = scroll_origin(8, 3, row_height, 4.0);
        let after = scroll_origin(8, 4, row_height, 4.0);
        assert!((origin - after - row_height).abs() < 0.01);
    }

    #[test]
    fn viewport_stays_when_history_and_offset_grow_together() {
        let origin = scroll_origin(8, 3, 20.0, 4.0);
        let after = scroll_origin(9, 4, 20.0, 4.0);
        assert!((origin - after).abs() < 0.01);
    }

    #[test]
    fn history_viewport_at_the_tip_clears_the_offset() {
        let tip = history_viewport(1200.0, 2000.0, 800.0, 20.0, 60, true);
        assert!(tip.stuck);
        assert_eq!(tip.frac, 0.0);
        assert_eq!(tip.offset, 0);
        let mid = history_viewport(800.0, 2000.0, 800.0, 20.0, 60, true);
        assert!(!mid.stuck);
        assert!((mid.frac - 0.0).abs() < 0.01);
        assert_eq!(mid.offset, 20);
    }

    #[test]
    fn a_lost_tip_offset_does_not_lock_onto_the_oldest_line() {
        let lost = history_viewport(0.0, 2000.0, 800.0, 20.0, 60, true);
        assert!(lost.stuck);
        assert_eq!(lost.offset, 0);
        let from_top = history_viewport(0.0, 2000.0, 800.0, 20.0, 60, false);
        assert!(!from_top.stuck);
        assert_eq!(from_top.offset, 60);
    }
}
