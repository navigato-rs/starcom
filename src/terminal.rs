use alacritty_terminal::{event, grid, index, selection, term, vte};

use crate::core;

const MAX_BUFFER_CELLS: usize = 131_072;
const MAX_HISTORY_LINES: usize = 10_000;

impl grid::Dimensions for core::Size {
    fn columns(&self) -> usize {
        (*self).columns()
    }

    fn screen_lines(&self) -> usize {
        (*self).rows()
    }

    fn total_lines(&self) -> usize {
        (*self).rows()
    }
}

/// A pane model with no local PTY and no renderer.
/// VoidListener intentionally suppresses device replies and side effects during
/// replay. tmux owns application-side terminal responses; never blindly forward
/// Alacritty's PtyWrite/clipboard events into send-keys.
pub struct Terminal {
    model: term::Term<event::VoidListener>,
    parser: vte::ansi::Processor,
    size: core::Size,
    history_limit: usize,
}

impl Terminal {
    pub fn new(size: core::Size, history_lines: usize) -> Self {
        let history_lines = history_lines
            .min(MAX_HISTORY_LINES)
            .min((MAX_BUFFER_CELLS / size.columns()).saturating_sub(size.rows()));
        let config = term::Config {
            scrolling_history: history_lines,
            osc52: term::Osc52::Disabled,
            ..term::Config::default()
        };
        Self {
            model: term::Term::new(config, &size, event::VoidListener),
            parser: vte::ansi::Processor::new(),
            size,
            history_limit: history_lines,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.model, bytes);
    }

    /// Conservative maximum cell allocation for the primary and alternate grids.
    pub fn estimated_cells(size: core::Size, history_lines: usize) -> usize {
        let history = history_lines
            .min(MAX_HISTORY_LINES)
            .min((MAX_BUFFER_CELLS / size.columns()).saturating_sub(size.rows()));
        (history + 2 * size.rows()) * size.columns()
    }

    pub fn history_capacity(&self) -> usize {
        self.history_limit
    }

    pub fn cell_budget(&self) -> usize {
        (self.history_limit + 2 * self.size.rows()) * self.size.columns()
    }

    /// Restore tmux's cursor convention, including x == width (pending wrap).
    /// Called only after State has validated these coordinates.
    pub(crate) fn restore_cursor(&mut self, column: usize, row: usize) {
        debug_assert!(column <= self.size.columns() && row < self.size.rows());
        let cursor = &mut self.model.grid_mut().cursor;
        cursor.point = index::Point::new(
            index::Line(row as i32),
            index::Column(column.min(self.size.columns() - 1)),
        );
        cursor.input_needs_wrap = column == self.size.columns();
    }

    pub fn size(&self) -> core::Size {
        self.size
    }

    /// Grow or shrink the local grid to the pane's pixel allocation.
    /// Does not change tmux; remote geometry is a separate, opt-in request.
    pub fn resize(&mut self, size: core::Size) {
        if size == self.size {
            return;
        }
        self.model.resize(size);
        self.size = size;
    }

    pub fn model(&self) -> &term::Term<event::VoidListener> {
        &self.model
    }

    /// Current active screen, not a reconstructed application transcript.
    /// The real renderer should use model().renderable_content(), not this
    /// allocation-heavy diagnostic representation.
    pub fn screen_lines(&self) -> Vec<String> {
        (0..self.size.rows())
            .map(|row| {
                self.model
                    .bounds_to_string(
                        index::Point::new(index::Line(row as i32), index::Column(0)),
                        index::Point::new(
                            index::Line(row as i32),
                            index::Column(self.size.columns() - 1),
                        ),
                    )
                    .trim_end_matches(&[' ', '\r', '\n'][..])
                    .to_owned()
            })
            .collect()
    }

    /// Selection remains in Alacritty's model so incoming scrolls rotate its
    /// anchors along with the text, including wide and combining characters.
    pub fn begin_selection(
        &mut self,
        point: index::Point,
        side: index::Side,
        kind: selection::SelectionType,
    ) {
        let point = self.clamp_point(point);
        self.model.selection = Some(selection::Selection::new(kind, point, side));
    }

    pub fn update_selection(&mut self, point: index::Point, side: index::Side) {
        let point = self.clamp_point(point);
        if let Some(ref mut selection) = self.model.selection {
            selection.update(point, side);
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.model.selection_to_string()
    }

    pub fn selection_range(&self) -> Option<selection::SelectionRange> {
        self.model
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.model))
    }

    pub fn clear_selection(&mut self) {
        self.model.selection = None;
    }

    /// OSC 8 URI on this cell, if the application set one.
    pub fn hyperlink_at(&self, point: index::Point) -> Option<String> {
        let point = self.clamp_point(point);
        self.model.grid()[point]
            .hyperlink()
            .map(|link| link.uri().to_owned())
    }

    fn clamp_point(&self, point: index::Point) -> index::Point {
        use grid::Dimensions;
        index::Point::new(
            index::Line(point.line.0.clamp(
                self.model.grid().topmost_line().0,
                self.size.rows() as i32 - 1,
            )),
            index::Column(point.column.0.min(self.size.columns() - 1)),
        )
    }

    pub fn is_alternate_screen(&self) -> bool {
        self.model.mode().contains(term::TermMode::ALT_SCREEN)
    }

    /// DECSET 1000/1002/1003. The application asked for mouse reports, so the
    /// wheel belongs to it rather than local history scrolling.
    pub fn reports_mouse(&self) -> bool {
        self.model.mode().intersects(term::TermMode::MOUSE_MODE)
    }

    pub fn sgr_mouse(&self) -> bool {
        self.model.mode().contains(term::TermMode::SGR_MOUSE)
    }

    /// Wheel should be delivered to the application: explicit mouse reporting,
    /// or xterm alternate-scroll (wheel becomes arrows on the alternate screen).
    pub fn wants_wheel(&self) -> bool {
        self.reports_mouse() || self.is_alternate_screen()
    }

    /// Lines between the live tip and the local history viewport. 0 follows
    /// new output; a positive value is how far the user has scrolled up.
    pub fn history_offset(&self) -> usize {
        self.model.grid().display_offset()
    }

    /// Pin the local history viewport. New output keeps this offset so a
    /// scrolled-up view does not walk with the live tip.
    pub fn scroll_history(&mut self, offset: usize) {
        use grid::Dimensions;
        let current = self.model.grid().display_offset();
        let target = offset.min(self.model.grid().history_size());
        let delta = target as i32 - current as i32;
        if delta != 0 {
            self.model
                .grid_mut()
                .scroll_display(grid::Scroll::Delta(delta));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_return_and_ansi_are_emulated_not_stripped() {
        let mut terminal = Terminal::new(core::Size::new(20, 4).unwrap(), 0);
        terminal.feed(b"working\r\x1b[2K\x1b[32mdone\x1b[0m");
        assert_eq!(terminal.screen_lines()[0], "done");
    }

    #[test]
    fn alternate_screen_restores_primary_content() {
        let mut terminal = Terminal::new(core::Size::new(20, 4).unwrap(), 0);
        terminal.feed(b"primary\x1b[?1049h\x1b[Halternate");
        assert!(terminal.is_alternate_screen());
        assert_eq!(terminal.screen_lines()[0], "alternate");
        terminal.feed(b"\x1b[?1049l");
        assert!(!terminal.is_alternate_screen());
        assert_eq!(terminal.screen_lines()[0], "primary");
    }

    #[test]
    fn local_resize_changes_the_grid() {
        let mut terminal = Terminal::new(core::Size::new(4, 2).unwrap(), 0);
        terminal.feed(b"abcdefgh");
        assert_eq!(terminal.screen_lines(), ["abcd", "efgh"]);
        terminal.resize(core::Size::new(8, 3).unwrap());
        assert_eq!(terminal.size(), core::Size::new(8, 3).unwrap());
        assert_eq!(terminal.screen_lines().len(), 3);
    }

    fn line_text(terminal: &Terminal, line: i32) -> String {
        let columns = terminal.size().columns();
        terminal
            .model
            .bounds_to_string(
                index::Point::new(index::Line(line), index::Column(0)),
                index::Point::new(index::Line(line), index::Column(columns.saturating_sub(1))),
            )
            .trim_end()
            .to_owned()
    }

    #[test]
    fn scrolled_history_stays_put_when_a_line_is_appended() {
        use grid::Dimensions;
        let mut terminal = Terminal::new(core::Size::new(20, 4).unwrap(), 8);
        for i in 0..12 {
            terminal.feed(format!("line{i:02}\r\n").as_bytes());
        }
        assert_eq!(terminal.model().grid().history_size(), 8);
        terminal.scroll_history(3);
        let offset = terminal.history_offset();
        assert_eq!(offset, 3);
        let shown = line_text(&terminal, -(offset as i32));
        assert!(!shown.is_empty(), "{shown:?}");
        terminal.feed(b"ping\r\n");
        let offset_after = terminal.history_offset();
        assert_eq!(offset_after, offset + 1);
        assert_eq!(line_text(&terminal, -(offset_after as i32)), shown);
    }

    #[test]
    fn osc8_hyperlink_is_stored_on_the_cell() {
        let mut terminal = Terminal::new(core::Size::new(20, 4).unwrap(), 0);
        terminal.feed(b"\x1b]8;;https://example.com/login\x07here\x1b]8;;\x07.");
        assert_eq!(
            terminal
                .hyperlink_at(index::Point::new(index::Line(0), index::Column(0)))
                .as_deref(),
            Some("https://example.com/login")
        );
        assert_eq!(
            terminal
                .hyperlink_at(index::Point::new(index::Line(0), index::Column(3)))
                .as_deref(),
            Some("https://example.com/login")
        );
        assert!(
            terminal
                .hyperlink_at(index::Point::new(index::Line(0), index::Column(4)))
                .is_none()
        );
    }

    #[test]
    fn mouse_reporting_and_alternate_scroll_are_observable() {
        let mut terminal = Terminal::new(core::Size::new(20, 4).unwrap(), 0);
        assert!(!terminal.reports_mouse());
        assert!(!terminal.wants_wheel());
        terminal.feed(b"\x1b[?1000h\x1b[?1006h");
        assert!(terminal.reports_mouse());
        assert!(terminal.sgr_mouse());
        assert!(terminal.wants_wheel());
        terminal.feed(b"\x1b[?1000l\x1b[?1006l\x1b[?1049h");
        assert!(!terminal.reports_mouse());
        assert!(terminal.is_alternate_screen());
        assert!(terminal.wants_wheel());
    }

    #[test]
    fn unicode_survives_bytewise_input() {
        let mut terminal = Terminal::new(core::Size::new(20, 4).unwrap(), 0);
        for byte in "café 界 e\u{301}".as_bytes() {
            terminal.feed(std::slice::from_ref(byte));
        }
        assert_eq!(terminal.screen_lines()[0], "café 界 e\u{301}");
    }
    #[test]
    fn local_copy_preserves_soft_wraps_wide_and_combining_characters() {
        let mut terminal = Terminal::new(core::Size::new(8, 3).unwrap(), 20);
        terminal.feed("abcdefghij界e\u{301}".as_bytes());
        terminal.begin_selection(
            index::Point::new(index::Line(0), index::Column(0)),
            index::Side::Left,
            selection::SelectionType::Simple,
        );
        terminal.update_selection(
            index::Point::new(index::Line(1), index::Column(4)),
            index::Side::Right,
        );
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("abcdefghij界e\u{301}")
        );
    }

    #[test]
    fn selection_moves_with_incoming_scroll_without_copying_new_text() {
        let mut terminal = Terminal::new(core::Size::new(10, 2).unwrap(), 20);
        terminal.feed(b"alpha\r\nbeta");
        terminal.begin_selection(
            index::Point::new(index::Line(0), index::Column(0)),
            index::Side::Left,
            selection::SelectionType::Simple,
        );
        terminal.update_selection(
            index::Point::new(index::Line(0), index::Column(4)),
            index::Side::Right,
        );
        terminal.feed(b"\r\ngamma");
        assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));
        assert_eq!(
            terminal.selection_range().unwrap().start.line,
            index::Line(-1)
        );
    }
}
