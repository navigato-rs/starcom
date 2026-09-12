//! User-originated terminal actions. tmux encodes special keys and bracketed
//! paste from its current application modes; no device replies are sent back.

use std::fmt;

use crate::{command, core};

pub const MAX_PASTE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Function(u8),
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub(crate) fn name(self, modifiers: Modifiers) -> Result<String, Error> {
        // Shift+Enter / Shift+Backspace have no portable terminal encoding.
        // Passing `S-Enter` or `S-BSpace` to tmux makes some tmux/application
        // combinations surface the key name as literal input. Treat them like
        // a conventional terminal does: the unshifted key.
        let modifiers = if matches!(self, Self::Enter | Self::Backspace) {
            Modifiers {
                shift: false,
                ..modifiers
            }
        } else {
            modifiers
        };
        let name = match self {
            Self::Enter => "Enter".to_owned(),
            Self::Backspace => "BSpace".to_owned(),
            Self::Tab if modifiers.shift && !modifiers.control && !modifiers.alt => {
                return Ok("BTab".to_owned());
            }
            Self::Tab => "Tab".to_owned(),
            Self::Escape => "Escape".to_owned(),
            Self::Up => "Up".to_owned(),
            Self::Down => "Down".to_owned(),
            Self::Left => "Left".to_owned(),
            Self::Right => "Right".to_owned(),
            Self::Home => "Home".to_owned(),
            Self::End => "End".to_owned(),
            Self::Insert => "IC".to_owned(),
            Self::Delete => "DC".to_owned(),
            Self::PageUp => "PPage".to_owned(),
            Self::PageDown => "NPage".to_owned(),
            Self::Function(number @ 1..=20) => format!("F{number}"),
            Self::Function(_) => return Err(Error::UnsupportedKey),
            // Never a tmux key name: unknown names are inserted as literal text.
            Self::WheelUp | Self::WheelDown => return Err(Error::UnsupportedKey),
        };
        Ok(format!(
            "{}{}{}{name}",
            if modifiers.control { "C-" } else { "" },
            if modifiers.alt { "M-" } else { "" },
            if modifiers.shift { "S-" } else { "" },
        ))
    }
}

/// Paste is text, not a second path for injecting escape sequences. Reject
/// controls (including ESC and C1) instead of silently stripping or executing
/// them. Deliberate control keys use Action::Bytes or Action::Key instead.
#[derive(Clone)]
pub struct Paste(String);

impl Paste {
    pub fn new(text: &str) -> Result<Self, Error> {
        if text.is_empty() || text.len() > MAX_PASTE_BYTES {
            return Err(Error::PasteSize);
        }
        if text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r'))
        {
            return Err(Error::PasteControl);
        }
        Ok(Self(text.replace("\r\n", "\n").replace('\r', "\n")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_multiline(&self) -> bool {
        self.0.contains('\n')
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Columns,
    Rows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resize {
    pub axis: Axis,
    pub cells: usize,
}

/// Intentionally not Debug: input and clipboard contents must not appear in logs.
#[derive(Clone)]
pub enum Action {
    Bytes(Vec<u8>),
    Key(Key, Modifiers),
    Paste(Paste),
    Resize(Resize),
    /// The GUI client's size in cells, from the same font metrics used to paint.
    ClientSize(core::Size),
    Split(Axis),
    KillPane,
    ZoomPane,
    SelectPane,
    /// Swap this pane with the neighbor `PaneId` in the same window.
    SwapPane(tmuxctl::PaneId),
}

impl Action {
    pub fn validate(&self) -> Result<(), Error> {
        match *self {
            Self::Bytes(ref bytes)
                if bytes.is_empty() || bytes.len() > command::MAX_INPUT_BYTES =>
            {
                Err(Error::InputSize)
            }
            Self::Key(key, modifiers) => key.name(modifiers).map(|_| ()),
            Self::Resize(Resize { cells, .. }) if !(1..=4096).contains(&cells) => {
                Err(Error::ResizeSize)
            }
            _ => Ok(()),
        }
    }

    pub fn changes_layout(&self) -> bool {
        matches!(
            self,
            Self::Resize(_)
                | Self::ClientSize(_)
                | Self::Split(_)
                | Self::KillPane
                | Self::ZoomPane
                | Self::SwapPane(_)
        )
    }

    pub fn changes_window_size(&self) -> bool {
        matches!(self, Self::Resize(_) | Self::ClientSize(_))
    }

    pub fn size(&self) -> usize {
        match *self {
            Self::Bytes(ref bytes) => bytes.len(),
            Self::Paste(ref paste) => paste.0.len(),
            Self::Key(..)
            | Self::Resize(..)
            | Self::ClientSize(_)
            | Self::Split(_)
            | Self::KillPane
            | Self::ZoomPane
            | Self::SelectPane
            | Self::SwapPane(_) => 32,
        }
    }
}

/// The exact pane geometry observed when the user made an action. All values
/// are typed; the server-side guard never interpolates user-provided syntax.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Target {
    pub session: tmuxctl::SessionId,
    pub pane: tmuxctl::PaneId,
    pub window: tmuxctl::WindowId,
    pub size: core::Size,
    pub left: usize,
    pub top: usize,
}

impl Target {
    pub(crate) fn guard(self, resizing: bool) -> String {
        let mut checks = vec![
            format!("#{{==:#{{session_id}},{}}}", self.session),
            format!("#{{==:#{{window_id}},{}}}", self.window),
            format!("#{{==:#{{pane_width}},{}}}", self.size.columns()),
            format!("#{{==:#{{pane_height}},{}}}", self.size.rows()),
            format!("#{{==:#{{pane_left}},{}}}", self.left),
            format!("#{{==:#{{pane_top}},{}}}", self.top),
            "#{==:#{pane_dead},0}".to_owned(),
            "#{==:#{pane_in_mode},0}".to_owned(),
            "#{==:#{pane_input_off},0}".to_owned(),
        ];
        if resizing {
            checks.push("#{==:#{window_zoomed_flag},0}".to_owned());
        } else {
            // send-keys honors synchronize-panes, so targeting one pane alone
            // does NOT prevent broadcasting. Never change that user's option.
            checks.push("#{==:#{synchronize-panes},0}".to_owned());
        }
        checks
            .into_iter()
            .reduce(|a, b| format!("#{{&&:{a},{b}}}"))
            .expect("nonempty")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    UnsupportedKey,
    InputSize,
    PasteSize,
    PasteControl,
    ResizeSize,
}

/// CSI cursor up/down used by xterm's alternate-scroll mode when the
/// application did not enable mouse reporting. These are terminal bytes, not
/// tmux key names: `send-keys Up` has different semantics in some applications.
pub fn alternate_scroll_bytes(up: bool) -> Vec<u8> {
    if up {
        b"\x1b[A".to_vec()
    } else {
        b"\x1b[B".to_vec()
    }
}

/// CSI mouse wheel report at a 0-based pane cell. Used when the application
/// enabled 1000/1002/1003 so local history scrolling would steal its input.
pub fn mouse_wheel_bytes(up: bool, column: usize, row: usize, sgr: bool) -> Vec<u8> {
    let x = column.saturating_add(1);
    let y = row.saturating_add(1);
    let button = if up { 64 } else { 65 };
    if sgr {
        format!("\x1b[<{button};{x};{y}M").into_bytes()
    } else {
        let encode = |n: usize| n.clamp(1, 223) as u8 + 32;
        vec![0x1b, b'[', b'M', button as u8 + 32, encode(x), encode(y)]
    }
}

/// Left-button press or release at a 0-based pane cell. Drags stay local.
///
/// SGR names the button in both directions and uses `M`/`m` for press/release.
/// X10 uses button 0 for press and button 3 for release.
pub fn mouse_click_bytes(press: bool, column: usize, row: usize, sgr: bool) -> Vec<u8> {
    let x = column.saturating_add(1);
    let y = row.saturating_add(1);
    if sgr {
        let suffix = if press { 'M' } else { 'm' };
        format!("\x1b[<0;{x};{y}{suffix}").into_bytes()
    } else {
        let encode = |n: usize| n.clamp(1, 223) as u8 + 32;
        let button = if press { 0 } else { 3 };
        vec![0x1b, b'[', b'M', button as u8 + 32, encode(x), encode(y)]
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Self::UnsupportedKey => "this key is not supported",
            Self::InputSize => "terminal input must contain 1..=1024 bytes",
            Self::PasteSize => "paste must contain 1..=65536 UTF-8 bytes",
            Self::PasteControl => "paste contains control characters; nothing was sent (only tabs and newlines are allowed)",
            Self::ResizeSize => "pane size must be between 1 and 4096 cells",
        })
    }
}
impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_cannot_become_control_commands() {
        assert_eq!(Key::Up.name(Modifiers::default()).unwrap(), "Up");
        assert_eq!(
            Key::Enter
                .name(Modifiers {
                    shift: true,
                    ..Modifiers::default()
                })
                .unwrap(),
            "Enter",
            "Shift+Enter must not become literal S-Enter input"
        );
        assert_eq!(
            Key::Backspace
                .name(Modifiers {
                    shift: true,
                    ..Modifiers::default()
                })
                .unwrap(),
            "BSpace",
            "Shift+Backspace must not become literal S-BSpace input"
        );
        assert_eq!(
            Key::Left
                .name(Modifiers {
                    control: true,
                    alt: true,
                    shift: true
                })
                .unwrap(),
            "C-M-S-Left"
        );
        assert_eq!(
            Key::Tab
                .name(Modifiers {
                    shift: true,
                    ..Modifiers::default()
                })
                .unwrap(),
            "BTab"
        );
        assert!(Key::Function(0).name(Modifiers::default()).is_err());
        assert!(Key::Function(21).name(Modifiers::default()).is_err());
        assert!(
            Key::WheelUp.name(Modifiers::default()).is_err(),
            "WheelUp must not be a send-keys name; tmux types unknown names as text"
        );
        assert!(Key::WheelDown.name(Modifiers::default()).is_err());
    }

    #[test]
    fn mouse_wheel_reports_sgr_and_x10() {
        assert_eq!(alternate_scroll_bytes(true), b"\x1b[A");
        assert_eq!(alternate_scroll_bytes(false), b"\x1b[B");
        assert_eq!(mouse_wheel_bytes(true, 0, 0, true), b"\x1b[<64;1;1M");
        assert_eq!(mouse_wheel_bytes(false, 9, 4, true), b"\x1b[<65;10;5M");
        assert_eq!(
            mouse_wheel_bytes(true, 0, 0, false),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
    }

    #[test]
    fn mouse_clicks_report_sgr_press_release_and_x10() {
        assert_eq!(mouse_click_bytes(true, 0, 0, true), b"\x1b[<0;1;1M");
        assert_eq!(mouse_click_bytes(false, 3, 7, true), b"\x1b[<0;4;8m");
        assert_eq!(
            mouse_click_bytes(true, 0, 0, false),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            mouse_click_bytes(false, 0, 0, false),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
    }

    #[test]
    fn paste_is_bounded_normalized_and_cannot_close_its_bracket() {
        let text = Paste::new("café\r\n界\re\u{301}\t").unwrap();
        assert_eq!(text.as_str(), "café\n界\ne\u{301}\t");
        assert!(text.is_multiline());
        for text in ["\x1b[201~echo pwned\n", "\0", "\u{9b}201~", "\x7f", "\x03"] {
            assert!(matches!(Paste::new(text), Err(Error::PasteControl)));
        }
        assert!(Paste::new("").is_err());
        assert!(Paste::new(&"x".repeat(MAX_PASTE_BYTES + 1)).is_err());
        assert!(Paste::new(&"x".repeat(MAX_PASTE_BYTES)).is_ok());
        assert!(
            !Paste::new("printf '%s' \"$(literal); #{pane_id}\"")
                .unwrap()
                .is_multiline()
        );
    }
}
