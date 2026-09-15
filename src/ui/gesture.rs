//! Pointer gesture state machine for a terminal pane.
//!
//! egui hands us a fistful of overlapping per-frame signals — `clicked`,
//! `drag_started`, `dragged`, `drag_stopped`, `double_clicked`, raw
//! `primary_pressed`/`primary_down` — and the terminal has to turn a whole
//! gesture (press … move … release, spanning several frames) into exactly one
//! of: copy a link, forward a click to the application, or drive local text
//! selection. Deciding that inline, per frame, from raw signals is what made
//! this code regress on every change: each branch re-derived the gesture and
//! the invariants lived only in the author's head.
//!
//! This module is the single place that decision lives. [`Pointer`] keeps the
//! one bit of cross-frame memory that egui does not (whether the gesture began
//! on a link), and [`Pointer::update`] maps a frame of [`Signals`] to one
//! [`Outcome`]. Both the input's mutually-exclusive facts and the output are
//! enums, so an invalid combination (a triple click that also forwards, a drag
//! that is somehow also a link copy) cannot be represented, and the UI applies
//! the result with an exhaustive `match`. It is pure and touches no egui types,
//! so every transition is unit-tested below rather than found by hand.

/// Which click egui completed this frame. The three are mutually exclusive:
/// egui reports the deepest one and suppresses the shallower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    Single,
    Double,
    Triple,
}

/// The phase of an in-progress drag. `Start` also reports as dragging in egui;
/// it is collapsed here so a frame carries exactly one phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    Start,
    Continue,
    Stop,
}

/// One frame of pointer input, reduced to the facts the decision needs. The UI
/// layer fills this in from the egui [`Response`](egui::Response) and the
/// terminal model; nothing here touches egui so the logic stays testable.
#[derive(Clone, Copy, Debug, Default)]
pub struct Signals {
    /// No Shift/Ctrl/Alt/Cmd held. Modified gestures stay local selection:
    /// they never copy a link or forward a click to the application.
    pub unmodified: bool,
    /// The primary button went down on this widget this frame. Sampled before
    /// egui has decided click-vs-drag, so a link copies even when a tiny move
    /// later turns the gesture into a drag.
    pub primary_pressed: bool,
    /// The primary button is held this frame. Its release (`false`) ends the
    /// gesture and clears the link latch.
    pub primary_down: bool,
    /// The click egui completed this frame, if any.
    pub click: Option<Click>,
    /// The phase of the drag in progress this frame, if any.
    pub drag: Option<DragPhase>,
    /// The press origin sits on a copyable link destination.
    pub link_at_origin: bool,
    /// The terminal currently holds selected text. Distinguishes a drag that
    /// produced a selection (copy it) from a collapsed one (forward a click).
    pub has_selection: bool,
    /// The application asked for mouse reports and the pane is live, so an
    /// unmodified tap belongs to it rather than to local selection.
    pub mouse_app: bool,
}

/// The single thing the UI should do this frame. Every representable value is
/// valid: `forward_to_app` rides only on the two gestures that can double as a
/// tap the application should see, and a link copy is its own variant that
/// nothing else can accompany.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do.
    Idle,
    /// Copy the link at the press origin and release any output hold.
    CopyLink,
    /// A completed single click: clear the selection and release the hold.
    /// `forward_to_app` also sends a paired mouse press+release at the cell.
    Click { forward_to_app: bool },
    /// Double click: select the word under the pointer, copy it, and clear.
    SelectWord,
    /// Triple click: select the line under the pointer, copy it, and clear.
    SelectLine,
    /// A drag began: hold output, anchor at the press origin, and extend to
    /// the pointer (a start frame is also a drag frame).
    DragBegin,
    /// A drag continued: extend the selection to the pointer.
    DragExtend,
    /// A drag ended: copy the selection if any, clear, and release the hold.
    /// `forward_to_app` forwards a click when the drag collapsed to one cell.
    DragEnd { forward_to_app: bool },
}

/// Per-pane gesture state. The only thing it remembers across frames is whether
/// the current press started on a link; everything else egui already tracks.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pointer {
    /// The in-progress gesture began on a link. Set on the press frame, held
    /// until the button comes back up, and suppresses selection and app clicks
    /// for the rest of the gesture so a drag off a link does not select.
    link: bool,
}

impl Pointer {
    /// Fold one frame of input into the gesture and return the action to run.
    pub fn update(&mut self, signals: Signals) -> Outcome {
        // The first frame of a gesture: the button just went down, or egui has
        // already collapsed a very fast press+release into a click/drag.
        let first_frame = signals.primary_pressed
            || signals.click == Some(Click::Single)
            || signals.drag == Some(DragPhase::Start);

        // A gesture that starts on a link is a link copy, full stop. Latch it
        // so the release frame and any intervening drag frames do nothing else.
        let already_latched = self.link;
        if !already_latched && signals.unmodified && first_frame && signals.link_at_origin {
            self.link = true;
        }

        let outcome = if self.link {
            // First latched frame copies; the rest of the gesture is inert.
            if already_latched {
                Outcome::Idle
            } else {
                Outcome::CopyLink
            }
        } else {
            classify(signals)
        };

        // The button is up: the gesture is over, so the next press starts fresh.
        if !signals.primary_down {
            self.link = false;
        }
        outcome
    }
}

/// A non-link gesture. Drag and click are mutually exclusive in egui, so at
/// most one family matches; a tap the application should see is a single click
/// or a collapsed drag in a live mouse pane.
fn classify(signals: Signals) -> Outcome {
    let taps_to_app = signals.mouse_app && signals.unmodified && !signals.has_selection;
    match signals.drag {
        Some(DragPhase::Start) => return Outcome::DragBegin,
        Some(DragPhase::Continue) => return Outcome::DragExtend,
        Some(DragPhase::Stop) => {
            return Outcome::DragEnd {
                forward_to_app: taps_to_app,
            };
        }
        None => {}
    }
    match signals.click {
        Some(Click::Triple) => Outcome::SelectLine,
        Some(Click::Double) => Outcome::SelectWord,
        Some(Click::Single) => Outcome::Click {
            forward_to_app: taps_to_app,
        },
        None => Outcome::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with the button held and nothing else, as a base for the cases.
    fn held() -> Signals {
        Signals {
            unmodified: true,
            primary_down: true,
            ..Signals::default()
        }
    }

    /// A release frame (button no longer down) with the given click.
    fn released(click: Click) -> Signals {
        Signals {
            click: Some(click),
            primary_down: false,
            ..held()
        }
    }

    #[test]
    fn a_plain_click_clears_selection_and_does_not_forward() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(released(Click::Single)),
            Outcome::Click {
                forward_to_app: false
            }
        );
    }

    #[test]
    fn double_and_triple_clicks_select_word_and_line() {
        let mut pointer = Pointer::default();
        assert_eq!(pointer.update(released(Click::Double)), Outcome::SelectWord);
        assert_eq!(pointer.update(released(Click::Triple)), Outcome::SelectLine);
    }

    #[test]
    fn a_drag_begins_extends_and_copies_on_release() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Start),
                ..held()
            }),
            Outcome::DragBegin
        );
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Continue),
                ..held()
            }),
            Outcome::DragExtend
        );
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Stop),
                has_selection: true,
                primary_down: false,
                ..held()
            }),
            Outcome::DragEnd {
                forward_to_app: false
            }
        );
    }

    #[test]
    fn a_press_on_a_link_copies_it_and_consumes_the_gesture() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                primary_pressed: true,
                link_at_origin: true,
                ..held()
            }),
            Outcome::CopyLink
        );
        // A drag off the link must not begin a selection.
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Start),
                link_at_origin: true,
                ..held()
            }),
            Outcome::Idle
        );
        // Release clears the latch; nothing else happens for the link.
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Stop),
                primary_down: false,
                ..held()
            }),
            Outcome::Idle
        );
    }

    #[test]
    fn a_modified_click_on_a_link_stays_local_selection() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                unmodified: false,
                link_at_origin: true,
                ..released(Click::Single)
            }),
            Outcome::Click {
                forward_to_app: false
            },
            "Shift/Ctrl/Alt/Cmd is a local gesture, even on a link"
        );
    }

    #[test]
    fn a_mouse_app_click_is_forwarded() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                mouse_app: true,
                ..released(Click::Single)
            }),
            Outcome::Click {
                forward_to_app: true
            }
        );
    }

    #[test]
    fn a_mouse_app_drag_that_selected_text_copies_instead_of_forwarding() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Stop),
                mouse_app: true,
                has_selection: true,
                primary_down: false,
                ..held()
            }),
            Outcome::DragEnd {
                forward_to_app: false
            }
        );
    }

    #[test]
    fn a_mouse_app_collapsed_drag_forwards_the_click() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Stop),
                mouse_app: true,
                has_selection: false,
                primary_down: false,
                ..held()
            }),
            Outcome::DragEnd {
                forward_to_app: true
            }
        );
    }

    #[test]
    fn a_modified_click_is_not_forwarded_to_the_app() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                unmodified: false,
                mouse_app: true,
                ..released(Click::Single)
            }),
            Outcome::Click {
                forward_to_app: false
            }
        );
    }

    #[test]
    fn double_and_triple_clicks_never_forward_even_in_a_mouse_app() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.update(Signals {
                mouse_app: true,
                ..released(Click::Double)
            }),
            Outcome::SelectWord
        );
        assert_eq!(
            pointer.update(Signals {
                mouse_app: true,
                ..released(Click::Triple)
            }),
            Outcome::SelectLine
        );
    }

    #[test]
    fn the_link_latch_resets_after_release_so_the_next_press_selects() {
        let mut pointer = Pointer::default();
        pointer.update(Signals {
            primary_pressed: true,
            link_at_origin: true,
            ..held()
        });
        pointer.update(Signals {
            primary_down: false,
            ..held()
        });
        // A later press on empty text drags normally.
        assert_eq!(
            pointer.update(Signals {
                drag: Some(DragPhase::Start),
                ..held()
            }),
            Outcome::DragBegin
        );
    }
}
