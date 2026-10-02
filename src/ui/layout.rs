//! Reconstruct the split hierarchy from tmux's pane rectangles.
//! Local dividers request a tmux resize on release, in the same cell units
//! used to paint. The server owns the final geometry.

use crate::{input, snapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
pub struct Boundary {
    pane: tmuxctl::PaneId,
    pane_cells: usize,
}

impl Boundary {
    /// Size from the GUI pixels and the same cell metrics used to paint.
    fn resize_pixels(
        self,
        axis: Axis,
        first_pixels: f32,
        cell: f32,
    ) -> Option<(tmuxctl::PaneId, input::Resize)> {
        if !(cell.is_finite() && cell > 0.0 && first_pixels.is_finite()) {
            return None;
        }
        let cells = (first_pixels / cell).round().clamp(1.0, 4096.0) as usize;
        (cells != self.pane_cells).then_some((
            self.pane,
            input::Resize {
                axis: match axis {
                    Axis::Horizontal => input::Axis::Columns,
                    Axis::Vertical => input::Axis::Rows,
                },
                cells,
            },
        ))
    }
}

#[derive(Debug)]
pub enum Node {
    Pane(tmuxctl::PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        boundary: Option<Boundary>,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    pub fn from_panes(panes: &[&snapshot::Pane]) -> Option<Self> {
        if panes.len() == 1 {
            return Some(Self::Pane(panes[0].state.pane));
        }
        if panes.is_empty() {
            return None;
        }
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let start = |pane: &&snapshot::Pane| match axis {
                Axis::Horizontal => pane.state.left,
                Axis::Vertical => pane.state.top,
            };
            let end = |pane: &&snapshot::Pane| {
                start(pane)
                    + match axis {
                        Axis::Horizontal => pane.state.size.columns(),
                        Axis::Vertical => pane.state.size.rows(),
                    }
            };
            let low = panes.iter().map(start).min()?;
            let high = panes.iter().map(end).max()?;
            let mut cuts: Vec<_> = panes.iter().map(start).filter(|&x| x > low).collect();
            cuts.sort_unstable_by_key(|&x| (x * 2).abs_diff(low + high));
            cuts.dedup();
            for cut in cuts {
                if panes
                    .iter()
                    .any(|pane| start(pane) < cut && end(pane) > cut)
                {
                    continue;
                }
                let (first, second): (Vec<_>, Vec<_>) =
                    panes.iter().copied().partition(|pane| start(pane) < cut);
                if first.is_empty() || second.is_empty() {
                    continue;
                }
                let first_end = first.iter().map(end).max()?;
                // A normal tmux split has exactly one separator cell. Unusual
                // border/status layouts remain viewable but cannot be resized
                // from an inferred boundary without a more complete layout model.
                // A leaf must span the entire first side along this axis.
                // Otherwise resize-pane can select a nearer nested split and
                // move the wrong boundary. Such dividers stay local-only.
                let boundary = if first_end + 1 == cut {
                    first
                        .iter()
                        .find(|pane| start(pane) == low && end(pane) == first_end)
                        .map(|pane| Boundary {
                            pane: pane.state.pane,
                            pane_cells: match axis {
                                Axis::Horizontal => pane.state.size.columns(),
                                Axis::Vertical => pane.state.size.rows(),
                            },
                        })
                } else {
                    None
                };
                return Some(Self::Split {
                    axis,
                    ratio: (first_end - low) as f32 / (high - low - (cut - first_end)) as f32,
                    boundary,
                    first: Box::new(Self::from_panes(&first)?),
                    second: Box::new(Self::from_panes(&second)?),
                });
            }
        }
        None
    }

    /// Zoomed tmux windows still list every pane, overlapping. Show the largest
    /// (the zoomed one filling the window) until tmux unzooms.
    pub fn from_panes_or_zoom(panes: &[&snapshot::Pane]) -> Option<Self> {
        Self::from_panes(panes).or_else(|| {
            panes
                .iter()
                .max_by_key(|pane| {
                    pane.state
                        .size
                        .columns()
                        .saturating_mul(pane.state.size.rows())
                })
                .map(|pane| Self::Pane(pane.state.pane))
        })
    }

    pub fn is_zoomed(panes: &[&snapshot::Pane]) -> bool {
        panes.len() > 1 && Self::from_panes(panes).is_none()
    }

    pub fn pane_ids(&self) -> Vec<tmuxctl::PaneId> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.pane_ids();
                ids.extend(second.pane_ids());
                ids
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        id: egui::Id,
        remote_resize: bool,
        cell_width: f32,
        row_height: f32,
        resizes: &mut Vec<(tmuxctl::PaneId, input::Resize)>,
        pane: &mut impl FnMut(&mut egui::Ui, egui::Rect, tmuxctl::PaneId),
    ) {
        match *self {
            Self::Pane(pane_id) => pane(ui, rect, pane_id),
            Self::Split {
                axis,
                ref mut ratio,
                boundary: remote_boundary,
                ref mut first,
                ref mut second,
            } => {
                let extent = match axis {
                    Axis::Horizontal => rect.width(),
                    Axis::Vertical => rect.height(),
                };
                let gap = 6.0_f32.min(extent.max(0.0));
                let available = (extent - gap).max(0.0);
                let minimum = 80.0_f32.min(available * 0.4);
                let boundary =
                    (available * *ratio).clamp(minimum, (available - minimum).max(minimum));
                let (mut a, mut divider, mut b) = (rect, rect, rect);
                match axis {
                    Axis::Horizontal => {
                        a.max.x = rect.min.x + boundary;
                        divider.min.x = a.max.x;
                        divider.max.x = a.max.x + gap;
                        b.min.x = divider.max.x;
                    }
                    Axis::Vertical => {
                        a.max.y = rect.min.y + boundary;
                        divider.min.y = a.max.y;
                        divider.max.y = a.max.y + gap;
                        b.min.y = divider.max.y;
                    }
                }
                // Divider id must not be `id.with(0)`: egui tooltips use
                // `widget_id.with(0)`, and the first child used to be that,
                // which panics (Tooltip vs Background) on a nested split.
                let response = ui
                    .interact(divider, id.with("div"), egui::Sense::drag())
                    .on_hover_cursor(match axis {
                        Axis::Horizontal => egui::CursorIcon::ResizeHorizontal,
                        Axis::Vertical => egui::CursorIcon::ResizeVertical,
                    })
                    .on_hover_text(if remote_resize && remote_boundary.is_some() {
                        "Release to resize the tmux pane. Other attached clients see the change."
                    } else {
                        "This divider cannot change tmux geometry right now."
                    });
                if response.dragged()
                    && available > 0.0
                    && let Some(position) = response.interact_pointer_pos()
                {
                    let position = match axis {
                        Axis::Horizontal => position.x - rect.min.x,
                        Axis::Vertical => position.y - rect.min.y,
                    };
                    *ratio = ((position - gap * 0.5) / available).clamp(0.05, 0.95);
                    ui.ctx().request_repaint();
                }
                if remote_resize
                    && response.drag_stopped()
                    && let Some(resize) = remote_boundary.and_then(|edge| {
                        let cell = match axis {
                            Axis::Horizontal => cell_width,
                            Axis::Vertical => row_height,
                        };
                        edge.resize_pixels(axis, boundary, cell)
                    })
                {
                    resizes.push(resize);
                }
                if response.hovered() || response.dragged() {
                    ui.painter().rect_filled(
                        divider.shrink(1.0),
                        1.0,
                        ui.visuals().widgets.hovered.bg_fill,
                    );
                }
                first.draw(
                    ui,
                    a,
                    id.with("a"),
                    remote_resize,
                    cell_width,
                    row_height,
                    resizes,
                    pane,
                );
                second.draw(
                    ui,
                    b,
                    id.with("b"),
                    remote_resize,
                    cell_width,
                    row_height,
                    resizes,
                    pane,
                );
            }
        }
    }
}

/// Cell rectangle of one pane, used to find a swap neighbor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaneRect {
    pub pane: tmuxctl::PaneId,
    pub window: tmuxctl::WindowId,
    pub left: usize,
    pub top: usize,
    pub columns: usize,
    pub rows: usize,
}

impl PaneRect {
    pub fn from_state(state: &snapshot::State) -> Self {
        Self {
            pane: state.pane,
            window: state.window,
            left: state.left,
            top: state.top,
            columns: state.size.columns(),
            rows: state.size.rows(),
        }
    }
}

/// Adjacent pane in each direction, if one shares an edge in the same window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Neighbors {
    pub left: Option<tmuxctl::PaneId>,
    pub right: Option<tmuxctl::PaneId>,
    pub up: Option<tmuxctl::PaneId>,
    pub down: Option<tmuxctl::PaneId>,
}

impl Neighbors {
    pub fn of(panes: impl IntoIterator<Item = PaneRect>, id: tmuxctl::PaneId) -> Self {
        let panes: Vec<_> = panes.into_iter().collect();
        let Some(me) = panes.iter().copied().find(|pane| pane.pane == id) else {
            return Self::default();
        };
        let left = me.left;
        let top = me.top;
        let right = left + me.columns;
        let bottom = top + me.rows;
        let mut found = Self::default();
        let mut best = [0_usize; 4];
        for pane in panes {
            if pane.pane == id || pane.window != me.window {
                continue;
            }
            let p_left = pane.left;
            let p_top = pane.top;
            let p_right = p_left + pane.columns;
            let p_bottom = p_top + pane.rows;
            let overlap_y = overlap(top, bottom, p_top, p_bottom);
            let overlap_x = overlap(left, right, p_left, p_right);
            // tmux reserves one cell between adjacent panes for its border.
            // Accept exact contact as well for synthetic layouts and versions
            // that report border-free coordinates.
            if touches(p_right, left) && overlap_y > best[0] {
                best[0] = overlap_y;
                found.left = Some(pane.pane);
            }
            if touches(right, p_left) && overlap_y > best[1] {
                best[1] = overlap_y;
                found.right = Some(pane.pane);
            }
            if touches(p_bottom, top) && overlap_x > best[2] {
                best[2] = overlap_x;
                found.up = Some(pane.pane);
            }
            if touches(bottom, p_top) && overlap_x > best[3] {
                best[3] = overlap_x;
                found.down = Some(pane.pane);
            }
        }
        found
    }

    pub fn count(self) -> usize {
        usize::from(self.left.is_some())
            + usize::from(self.right.is_some())
            + usize::from(self.up.is_some())
            + usize::from(self.down.is_some())
    }
}

fn overlap(a0: usize, a1: usize, b0: usize, b1: usize) -> usize {
    a1.min(b1).saturating_sub(a0.max(b0))
}

fn touches(before_end: usize, after_start: usize) -> bool {
    before_end == after_start || before_end.checked_add(1) == Some(after_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn divider_changes_one_axis_relative_to_observed_cells() {
        let boundary = Boundary {
            pane: tmuxctl::PaneId(2),
            pane_cells: 40,
        };
        let (pane, resize) = boundary
            .resize_pixels(Axis::Horizontal, 400.0, 8.0)
            .unwrap();
        assert_eq!(pane, tmuxctl::PaneId(2));
        assert_eq!(
            resize,
            input::Resize {
                axis: input::Axis::Columns,
                cells: 50
            }
        );
        assert!(
            boundary
                .resize_pixels(Axis::Horizontal, 320.0, 8.0)
                .is_none()
        );
        assert!(
            boundary
                .resize_pixels(Axis::Vertical, 10.0, f32::NAN)
                .is_none()
        );
    }

    #[test]
    fn demo_recovers_independent_window_splits() {
        let view = crate::desktop::demo_view().unwrap();
        let panes: Vec<_> = view
            .panes()
            .values()
            .filter(|pane| pane.state.window == tmuxctl::WindowId(0))
            .collect();
        let node = Node::from_panes(&panes).unwrap();
        match node {
            Node::Split {
                axis: Axis::Horizontal,
                ratio,
                ..
            } => assert!((0.49..0.52).contains(&ratio)),
            _ => panic!("expected side-by-side split"),
        }
    }

    fn pane(id: u32, left: usize, top: usize, columns: usize, rows: usize) -> snapshot::Pane {
        let state = snapshot::State::parse(&format!(
            "%{id}|@0|{columns}|{rows}|{left}|{top}|0|0|0|0|2000|||0|{}|1|0|0|0|1|0|0|0|0|0|1|",
            rows - 1
        ))
        .unwrap();
        let size = state.size;
        snapshot::Pane {
            state,
            terminal: crate::terminal::Terminal::new(size, 0),
            history_may_be_truncated: false,
        }
    }

    #[test]
    fn overlapping_panes_are_treated_as_a_zoomed_window() {
        let zoomed = pane(0, 0, 0, 80, 24);
        let hidden = pane(1, 0, 0, 40, 24);
        let panes = [&zoomed, &hidden];
        assert!(
            Node::from_panes(&panes).is_none(),
            "overlapping geometry is not a split"
        );
        match Node::from_panes_or_zoom(&panes) {
            Some(Node::Pane(id)) => assert_eq!(id, tmuxctl::PaneId(0)),
            other => panic!("expected the larger pane, got {other:?}"),
        }
        assert!(Node::is_zoomed(&panes));
        assert_eq!(
            Neighbors::of(
                panes.iter().map(|pane| PaneRect::from_state(&pane.state)),
                tmuxctl::PaneId(0)
            ),
            Neighbors::default(),
            "overlapping panes do not share an edge"
        );
    }

    #[test]
    fn a_side_by_side_split_has_left_and_right_neighbors() {
        let left = pane(1, 0, 0, 39, 24);
        let right = pane(2, 40, 0, 40, 24);
        let rects = [
            PaneRect::from_state(&left.state),
            PaneRect::from_state(&right.state),
        ];
        assert_eq!(
            Neighbors::of(rects, tmuxctl::PaneId(1)),
            Neighbors {
                right: Some(tmuxctl::PaneId(2)),
                ..Neighbors::default()
            }
        );
        assert_eq!(
            Neighbors::of(rects, tmuxctl::PaneId(2)),
            Neighbors {
                left: Some(tmuxctl::PaneId(1)),
                ..Neighbors::default()
            }
        );
    }

    #[test]
    fn a_stacked_split_has_up_and_down_neighbors() {
        let top = pane(1, 0, 0, 80, 10);
        let bottom = pane(2, 0, 11, 80, 13);
        let rects = [
            PaneRect::from_state(&top.state),
            PaneRect::from_state(&bottom.state),
        ];
        assert_eq!(
            Neighbors::of(rects, tmuxctl::PaneId(1)).down,
            Some(tmuxctl::PaneId(2))
        );
        assert_eq!(
            Neighbors::of(rects, tmuxctl::PaneId(2)).up,
            Some(tmuxctl::PaneId(1))
        );
    }
}
