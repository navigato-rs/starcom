use std::fmt;

/// Initial safety budget, not a terminal-protocol limit.
pub const MAX_VISIBLE_CELLS: usize = 65_536;
pub const MAX_USER_OPTIONS: usize = 64;
pub const MAX_USER_OPTION_NAME: usize = 64;
pub const MAX_USER_OPTION_VALUE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Size {
    columns: usize,
    rows: usize,
}

impl Size {
    pub fn new(columns: usize, rows: usize) -> Result<Self, InvalidValue> {
        if columns == 0 || rows == 0 || columns > 4096 || rows > 4096 {
            return Err(InvalidValue::Size);
        }
        if columns * rows > MAX_VISIBLE_CELLS {
            return Err(InvalidValue::Size);
        }
        Ok(Self { columns, rows })
    }

    pub fn columns(self) -> usize {
        self.columns
    }

    pub fn rows(self) -> usize {
        self.rows
    }

    /// tmux reports x == width when a wrap is pending. Saved alternate-screen
    /// coordinates are not updated on resize, so they can sit past the current
    /// pane; clamp rather than failing the snapshot.
    pub fn clamp_cursor(self, column: usize, row: usize) -> (usize, usize) {
        (column.min(self.columns()), row.min(self.rows() - 1))
    }
}

impl Default for Size {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
        }
    }
}

/// A name, never a shell fragment or a fuzzy tmux target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionName(String);

impl SessionName {
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidValue> {
        let name = name.into();
        if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
            return Err(InvalidValue::SessionName);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A deliberately conservative tmux user-option name. Tmux itself accepts
/// any name beginning with `@`; Starcom keeps names easy to type, display, and
/// target by limiting the suffix to common key characters.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserOptionName(String);

impl UserOptionName {
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidValue> {
        let mut name = name.into();
        if !name.starts_with('@') {
            name.insert(0, '@');
        }
        let suffix = &name[1..];
        if suffix.is_empty()
            || suffix.len() > MAX_USER_OPTION_NAME
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(InvalidValue::UserOptionName);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn suffix(&self) -> &str {
        &self.0[1..]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserOption {
    pub name: UserOptionName,
    pub value: String,
}

impl UserOption {
    pub fn new(name: UserOptionName, value: String) -> Result<Self, InvalidValue> {
        if value.len() > MAX_USER_OPTION_VALUE || value.chars().any(char::is_control) {
            return Err(InvalidValue::UserOptionValue);
        }
        Ok(Self { name, value })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidValue {
    Size,
    SessionName,
    UserOptionName,
    UserOptionValue,
}

impl fmt::Display for InvalidValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match *self {
            Self::Size => "terminal dimensions are zero or exceed the cell budget",
            Self::SessionName => "session name is empty, too long, or contains control characters",
            Self::UserOptionName => {
                "option name must use 1–64 ASCII letters, digits, '.', '_', or '-'"
            }
            Self::UserOptionValue => {
                "option value exceeds 4096 bytes or contains control characters"
            }
        })
    }
}

impl std::error::Error for InvalidValue {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_are_bounded_before_allocation() {
        assert!(Size::new(0, 24).is_err());
        assert!(Size::new(80, 0).is_err());
        assert!(Size::new(usize::MAX, 2).is_err());
        assert!(Size::new(4096, 4096).is_err());
        assert_eq!(Size::new(80, 24).unwrap(), Size::default());
        assert_eq!(Size::default().clamp_cursor(80, 23), (80, 23));
        assert_eq!(Size::default().clamp_cursor(120, 40), (80, 23));
    }

    #[test]
    fn session_names_reject_control_characters() {
        for name in ["", "work\nkill-server", "work\r", "work\0", "work\u{1b}"] {
            assert!(SessionName::new(name).is_err());
        }
        assert!(SessionName::new("a".repeat(1025)).is_err());
        assert!(SessionName::new("work with spaces; $(not-a-command)").is_ok());
    }

    #[test]
    fn user_option_names_have_a_small_predictable_alphabet() {
        assert_eq!(
            UserOptionName::new("project.id").unwrap().as_str(),
            "@project.id"
        );
        assert_eq!(
            UserOptionName::new("@build-kind").unwrap().suffix(),
            "build-kind"
        );
        for name in [
            "",
            "@",
            "has space",
            "semi;colon",
            "bracket[0]",
            "snowman-☃",
        ] {
            assert!(UserOptionName::new(name).is_err(), "accepted {name:?}");
        }
        assert!(UserOptionName::new("x".repeat(MAX_USER_OPTION_NAME)).is_ok());
        assert!(UserOptionName::new("x".repeat(MAX_USER_OPTION_NAME + 1)).is_err());
    }

    #[test]
    fn user_option_values_are_single_line_and_bounded() {
        let name = UserOptionName::new("note").unwrap();
        assert!(UserOption::new(name.clone(), "spaces and symbols: $()".into()).is_ok());
        assert!(UserOption::new(name.clone(), "line\nbreak".into()).is_err());
        assert!(UserOption::new(name, "x".repeat(MAX_USER_OPTION_VALUE + 1)).is_err());
    }
}
