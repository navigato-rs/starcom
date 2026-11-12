//! Kitty OSC 22 pointer shapes.
//!
//! The sequence is `ESC ] 22 ; <op><names> ST`, where ST is BEL or `ESC \`.
//! A leading `>` pushes, `<` pops, `?` is a query (ignored: this model never
//! writes a reply), and `=` or no operator sets the shape. `default` and an
//! empty payload hand the pointer back to the terminal. The main and alternate
//! screens have separate stacks, and a full reset empties both.

const STACK_LIMIT: usize = 16;
/// A full push is one operator plus sixteen names. The longest name we accept
/// is `vertical-text`, so this is enough for that list and a few fallbacks.
const MAX_PAYLOAD: usize = 512;
const PREFIX: &[u8] = b"22;";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Alias,
    Cell,
    Copy,
    Crosshair,
    EastWest,
    East,
    Grab,
    Grabbing,
    Help,
    Move,
    North,
    NorthEast,
    NorthEastSouthWest,
    NoDrop,
    NotAllowed,
    NorthSouth,
    NorthWest,
    NorthWestSouthEast,
    Pointer,
    Progress,
    South,
    SouthEast,
    SouthWest,
    Text,
    VerticalText,
    West,
    Wait,
    ZoomIn,
    ZoomOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Known {
    Shape(Shape),
    /// CSS `default`: the terminal chooses, same as an empty stack.
    Terminal,
    /// Not a shape we know. A set skips it and tries the next fallback.
    Skip,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
    /// ESC seen inside an OSC, waiting to see if it is `ESC \`.
    OscEsc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OscKind {
    /// Still matching the `22;` prefix.
    Prefix,
    Body,
    Skip,
}

struct Osc {
    kind: OscKind,
    matched: u8,
    overflow: bool,
    buf: String,
}

impl Osc {
    fn begin(&mut self) {
        self.kind = OscKind::Prefix;
        self.matched = 0;
        self.overflow = false;
        self.buf.clear();
    }

    fn push(&mut self, byte: u8) {
        match self.kind {
            OscKind::Skip => {}
            OscKind::Prefix => {
                if byte == PREFIX[usize::from(self.matched)] {
                    self.matched += 1;
                    if usize::from(self.matched) == PREFIX.len() {
                        self.kind = OscKind::Body;
                    }
                } else {
                    self.kind = OscKind::Skip;
                }
            }
            OscKind::Body => {
                if self.buf.len() >= MAX_PAYLOAD {
                    self.overflow = true;
                } else {
                    self.buf.push(char::from(byte));
                }
            }
        }
    }
}

#[derive(Default)]
struct Csi {
    private: bool,
    /// `?` is only legal as the first byte.
    started: bool,
    param: u32,
    saw_1049: bool,
    intermediate: u8,
}

impl Csi {
    fn begin(&mut self) {
        *self = Self::default();
    }

    fn commit_param(&mut self) {
        if self.private && self.param == 1049 {
            self.saw_1049 = true;
        }
        self.param = 0;
    }
}

#[derive(Default)]
pub struct Shapes {
    main: Vec<Shape>,
    alt: Vec<Shape>,
    on_alt: bool,
    phase: Phase,
    osc: Osc,
    csi: Csi,
}

impl Default for Osc {
    fn default() -> Self {
        Self {
            kind: OscKind::Skip,
            matched: 0,
            overflow: false,
            buf: String::new(),
        }
    }
}

impl Shapes {
    pub fn scan(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    /// Shape for the screen that is actually visible. Empty means the terminal
    /// chooses (I-beam, or an arrow while the application tracks the mouse).
    pub fn get(&self, alternate: bool) -> Option<Shape> {
        let stack = if alternate { &self.alt } else { &self.main };
        stack.last().copied()
    }

    fn byte(&mut self, byte: u8) {
        // CAN and SUB cancel any sequence in progress.
        if byte == 0x18 || byte == 0x1a {
            self.phase = Phase::Ground;
            return;
        }
        match self.phase {
            Phase::Ground => {
                if byte == 0x1b {
                    self.phase = Phase::Esc;
                }
            }
            Phase::Esc => self.escape(byte),
            Phase::Csi => self.csi(byte),
            Phase::Osc => {
                if byte == 0x07 {
                    self.finish_osc();
                } else if byte == 0x1b {
                    self.phase = Phase::OscEsc;
                } else if byte != 0x00 {
                    self.osc.push(byte);
                }
            }
            Phase::OscEsc => {
                if byte == b'\\' {
                    self.finish_osc();
                } else {
                    // The ESC aborted the OSC and starts a new escape.
                    self.phase = Phase::Esc;
                    self.escape(byte);
                }
            }
        }
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            0x1b => {}
            b'c' => {
                self.main.clear();
                self.alt.clear();
                self.on_alt = false;
                self.phase = Phase::Ground;
            }
            b'[' => {
                self.csi.begin();
                self.phase = Phase::Csi;
            }
            b']' => {
                self.osc.begin();
                self.phase = Phase::Osc;
            }
            _ => self.phase = Phase::Ground,
        }
    }

    fn csi(&mut self, byte: u8) {
        if byte == 0x1b {
            self.phase = Phase::Esc;
            return;
        }
        match byte {
            b'0'..=b'9' => {
                self.csi.started = true;
                self.csi.param = self
                    .csi
                    .param
                    .saturating_mul(10)
                    .saturating_add(u32::from(byte - b'0'));
            }
            b';' => {
                self.csi.started = true;
                self.csi.commit_param();
            }
            b'?' if !self.csi.started && self.csi.intermediate == 0 => {
                self.csi.private = true;
                self.csi.started = true;
            }
            0x20..=0x2f => {
                self.csi.intermediate = byte;
                self.csi.started = true;
            }
            0x40..=0x7e => {
                self.csi.commit_param();
                if self.csi.private && (byte == b'h' || byte == b'l') && self.csi.saw_1049 {
                    self.on_alt = byte == b'h';
                }
                // DECSTR. A full reset also clears both stacks; soft reset does
                // not leave the alternate screen, so the screen flag stays.
                if !self.csi.private && self.csi.intermediate == b'!' && byte == b'p' {
                    self.main.clear();
                    self.alt.clear();
                }
                self.phase = Phase::Ground;
            }
            _ => self.phase = Phase::Ground,
        }
    }

    fn finish_osc(&mut self) {
        if self.osc.kind == OscKind::Body && !self.osc.overflow {
            let body = std::mem::take(&mut self.osc.buf);
            self.apply(&body);
        }
        self.phase = Phase::Ground;
    }

    fn apply(&mut self, body: &str) {
        if body.is_empty() {
            self.clear_current();
            return;
        }
        let (push, pop, list) = match body.as_bytes()[0] {
            b'>' => (true, false, &body[1..]),
            b'<' => (false, true, &body[1..]),
            b'?' => return,
            b'=' => (false, false, &body[1..]),
            _ => (false, false, body),
        };
        if pop {
            self.stack_mut().pop();
            return;
        }
        if push {
            for name in list.split(',') {
                let Known::Shape(shape) = lookup(name) else {
                    continue;
                };
                let stack = self.stack_mut();
                if stack.len() == STACK_LIMIT {
                    stack.remove(0);
                }
                stack.push(shape);
            }
            return;
        }
        if list.is_empty() {
            self.clear_current();
            return;
        }
        for name in list.split(',') {
            match lookup(name) {
                Known::Shape(shape) => {
                    let stack = self.stack_mut();
                    stack.clear();
                    stack.push(shape);
                    return;
                }
                Known::Terminal => {
                    self.clear_current();
                    return;
                }
                Known::Skip => {}
            }
        }
    }

    fn clear_current(&mut self) {
        self.stack_mut().clear();
    }

    fn stack_mut(&mut self) -> &mut Vec<Shape> {
        if self.on_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }
}

fn lookup(name: &str) -> Known {
    match name {
        "default" | "left_ptr" | "arrow" => Known::Terminal,
        "alias" => Known::Shape(Shape::Alias),
        "cell" => Known::Shape(Shape::Cell),
        "copy" => Known::Shape(Shape::Copy),
        "crosshair" | "cross" => Known::Shape(Shape::Crosshair),
        "e-resize" => Known::Shape(Shape::East),
        "ew-resize" => Known::Shape(Shape::EastWest),
        "grab" => Known::Shape(Shape::Grab),
        "grabbing" => Known::Shape(Shape::Grabbing),
        "help" => Known::Shape(Shape::Help),
        "move" => Known::Shape(Shape::Move),
        "n-resize" => Known::Shape(Shape::North),
        "ne-resize" => Known::Shape(Shape::NorthEast),
        "nesw-resize" => Known::Shape(Shape::NorthEastSouthWest),
        "no-drop" => Known::Shape(Shape::NoDrop),
        "not-allowed" => Known::Shape(Shape::NotAllowed),
        "ns-resize" => Known::Shape(Shape::NorthSouth),
        "nw-resize" => Known::Shape(Shape::NorthWest),
        "nwse-resize" => Known::Shape(Shape::NorthWestSouthEast),
        "pointer" | "hand" | "hand1" | "hand2" => Known::Shape(Shape::Pointer),
        "progress" => Known::Shape(Shape::Progress),
        "s-resize" => Known::Shape(Shape::South),
        "se-resize" => Known::Shape(Shape::SouthEast),
        "sw-resize" => Known::Shape(Shape::SouthWest),
        "text" | "ibeam" | "xterm" => Known::Shape(Shape::Text),
        "vertical-text" => Known::Shape(Shape::VerticalText),
        "w-resize" => Known::Shape(Shape::West),
        "wait" | "watch" => Known::Shape(Shape::Wait),
        "zoom-in" => Known::Shape(Shape::ZoomIn),
        "zoom-out" => Known::Shape(Shape::ZoomOut),
        _ => Known::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(bytes: &[&[u8]]) -> Shapes {
        let mut shapes = Shapes::default();
        for chunk in bytes {
            shapes.scan(chunk);
        }
        shapes
    }

    #[test]
    fn set_pointer_and_hand_control_back() {
        let mut shapes = scanned(&[b"\x1b]22;pointer\x1b\\"]);
        assert_eq!(shapes.get(false), Some(Shape::Pointer));
        shapes.scan(b"\x1b]22;default\x07");
        assert_eq!(shapes.get(false), None);
        shapes.scan(b"\x1b]22;pointer\x1b\\");
        shapes.scan(b"\x1b]22;\x1b\\");
        assert_eq!(shapes.get(false), None);
    }

    #[test]
    fn set_uses_the_first_supported_name() {
        let shapes = scanned(&[b"\x1b]22;nope,pointer\x1b\\"]);
        assert_eq!(shapes.get(false), Some(Shape::Pointer));
        let shapes = scanned(&[b"\x1b]22;=text\x1b\\"]);
        assert_eq!(shapes.get(false), Some(Shape::Text));
    }

    #[test]
    fn push_pops_and_stops_at_sixteen() {
        let mut shapes = Shapes::default();
        shapes.scan(b"\x1b]22;>wait,pointer\x1b\\");
        assert_eq!(shapes.get(false), Some(Shape::Pointer));
        shapes.scan(b"\x1b]22;<ignored\x1b\\");
        assert_eq!(shapes.get(false), Some(Shape::Wait));
        shapes.scan(b"\x1b]22;<\x1b\\");
        assert_eq!(shapes.get(false), None);
        shapes.scan(b"\x1b]22;<\x1b\\");
        assert_eq!(shapes.get(false), None);

        let mut body = String::from("\x1b]22;>");
        for index in 0..20 {
            if index > 0 {
                body.push(',');
            }
            body.push_str(if index % 2 == 0 { "wait" } else { "pointer" });
        }
        body.push_str("\x1b\\");
        shapes.scan(body.as_bytes());
        assert_eq!(shapes.main.len(), STACK_LIMIT);
        // The oldest four (wait, pointer, wait, pointer) were evicted, so the
        // bottom of the remaining stack is the fifth push, which is wait.
        assert_eq!(shapes.main[0], Shape::Wait);
        assert_eq!(shapes.get(false), Some(Shape::Pointer));
    }

    #[test]
    fn alternate_screen_and_reset_keep_their_own_stacks() {
        let mut shapes = scanned(&[b"\x1b]22;text\x1b\\"]);
        shapes.scan(b"\x1b[?1049h\x1b]22;pointer\x1b\\");
        assert_eq!(shapes.get(false), Some(Shape::Text));
        assert_eq!(shapes.get(true), Some(Shape::Pointer));
        shapes.scan(b"\x1b[?1049l");
        assert_eq!(shapes.get(false), Some(Shape::Text));
        shapes.scan(b"\x1b[?1049h");
        assert_eq!(shapes.get(true), Some(Shape::Pointer));
        shapes.scan(b"\x1bc");
        assert_eq!(shapes.get(false), None);
        assert_eq!(shapes.get(true), None);
    }

    #[test]
    fn a_query_and_a_title_do_not_change_the_shape() {
        let mut shapes = scanned(&[b"\x1b]22;pointer\x1b\\"]);
        shapes.scan(b"\x1b]22;?pointer,text\x1b\\");
        shapes.scan(b"\x1b]0;title\x07\x1b]2;not a shape\x1b\\");
        assert_eq!(shapes.get(false), Some(Shape::Pointer));
    }

    #[test]
    fn a_shape_split_across_feeds_still_joins() {
        let shapes = scanned(&[b"\x1b]22;poi", b"nter\x1b\\"]);
        assert_eq!(shapes.get(false), Some(Shape::Pointer));
    }
}
