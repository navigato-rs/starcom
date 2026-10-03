//! Persisted, non-secret connection tabs.
//!
//! This file records where to connect and how, never anything that would let a
//! reader connect: no keys, no passphrases, no agent handles, no host-key
//! material, or terminal contents. A last-used session name is a resume target
//! only. Resuming a tab reconnects to that saved destination and existing tmux
//! session using the current SSH configuration.
//!
//! The format is a few bounded lines rather than a serialization dependency,
//! for the same reason the SSH-config reader is: reading it must not be able to
//! run anything, and every limit must be visible in one place.

use std::{collections, env, fs, io, path};

use anyhow::Context;

/// Refuse a file that has grown beyond anything this program writes.
const MAX_BYTES: u64 = 64 * 1024;
const MAX_VALUE: usize = 4096;
/// A key, a space, and the longest value this program writes.
const MAX_LINE: usize = MAX_VALUE + 128;
pub const MAX_TABS: usize = 16;
/// Desktop redraw cap written as a file-level setting. 0 in a hand-edited file
/// means "use the default" rather than "never paint".
pub const DEFAULT_FPS: u32 = 5;
pub const MAX_FPS: u32 = 60;
/// Seconds a healthy tab may sit without a view change before its row turns
/// blue. 0 disables the hint.
pub const DEFAULT_IDLE: u32 = 30;
pub const MAX_IDLE: u32 = 3600;
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 184;
pub const MIN_SIDEBAR_WIDTH: u16 = 120;
pub const MAX_SIDEBAR_WIDTH: u16 = 320;
pub const DEFAULT_FONT_SIZE: u16 = 14;
pub const MIN_FONT_SIZE: u16 = 10;
pub const MAX_FONT_SIZE: u16 = 28;
pub const DEFAULT_HISTORY: usize = 1000;
/// Cumulative seconds the app has been open. Caps so a corrupt file cannot
/// invent an absurd lifetime.
pub const MAX_OPEN_SECS: u64 = 100 * 365 * 24 * 60 * 60;

pub fn clamp_fps(fps: u32) -> u32 {
    if fps == 0 {
        DEFAULT_FPS
    } else {
        fps.min(MAX_FPS)
    }
}

pub fn clamp_sidebar_width(width: u16) -> u16 {
    width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
}

pub fn clamp_font_size(size: u16) -> u16 {
    size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

/// One saved tab. Every field is a destination or a preference; none of it is
/// a credential. Paths name a file the user already chose, never its contents.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tab {
    pub destination: String,
    pub host: String,
    pub user: String,
    /// Last session this tab attached to, used as the startup resume target.
    pub session: String,
    /// Marker parsed from v0.3 files. Current workspaces ignore these local
    /// records; remote discovery handles every non-managed session uniformly.
    pub legacy: bool,
    /// Last tmux window and pane selected in this tab. These are hints only:
    /// reconnect validates them against the new snapshot before using them.
    pub window: Option<u32>,
    pub pane: Option<u32>,
    pub port: u16,
    pub identity: String,
    pub known_hosts: String,
    pub socket: String,
    pub history: usize,
    pub interactive: bool,
    pub reconnect: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub restore_tabs: bool,
    pub fps: u32,
    pub idle: u32,
    pub sidebar_width: u16,
    pub font_size: u16,
    pub open_secs: u64,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            restore_tabs: true,
            fps: DEFAULT_FPS,
            idle: DEFAULT_IDLE,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            font_size: DEFAULT_FONT_SIZE,
            open_secs: 0,
        }
    }
}

/// `~/.config/starcom/workspace.conf`, or the platform equivalent. Returns None
/// when there is no home directory to anchor it to; the desktop then runs
/// without persistence rather than inventing a location.
pub fn path(home: &path::Path) -> path::PathBuf {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(path::PathBuf::from)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(path::PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    base.unwrap_or_else(|| home.join(".config"))
        .join("starcom")
        .join("workspace.conf")
}

/// A missing file is the normal first-run state. An existing but unreadable or
/// malformed file is an error the UI reports; nothing is invented to replace it.
pub fn load(file: &path::Path) -> anyhow::Result<Option<Workspace>> {
    let data = match fs::metadata(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        _ => {
            let mut data = String::new();
            io::Read::read_to_string(
                &mut io::Read::take(fs::File::open(file)?, MAX_BYTES + 1),
                &mut data,
            )
            .with_context(|| format!("read {}", file.display()))?;
            anyhow::ensure!(
                data.len() as u64 <= MAX_BYTES,
                "saved workspace exceeds {MAX_BYTES} bytes"
            );
            data
        }
    };
    parse(&data).map(Some)
}

fn parse(text: &str) -> anyhow::Result<Workspace> {
    let mut workspace = Workspace::default();
    let mut version = 1_u32;
    let mut fields: Option<collections::BTreeMap<&str, &str>> = None;
    for (number, line) in text.lines().enumerate() {
        let where_ = || format!("line {}", number + 1);
        anyhow::ensure!(line.len() <= MAX_LINE, "{}: line too long", where_());
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[tab]" {
            anyhow::ensure!(
                workspace.tabs.len() < MAX_TABS,
                "saved workspace holds more than {MAX_TABS} tabs"
            );
            if let Some(fields) = fields.take() {
                workspace
                    .tabs
                    .push(tab(&fields, version).with_context(where_)?);
            }
            fields = Some(collections::BTreeMap::new());
            continue;
        }
        let (key, value) = line
            .split_once(' ')
            .map_or((line, ""), |(key, value)| (key, value.trim_start()));
        anyhow::ensure!(value.len() <= MAX_VALUE, "{}: value too long", where_());
        anyhow::ensure!(
            !value.chars().any(char::is_control),
            "{}: control character in value",
            where_()
        );
        match fields {
            Some(ref mut fields) => {
                anyhow::ensure!(
                    fields.insert(key, value).is_none(),
                    "{}: duplicate key {key}",
                    where_()
                );
            }
            // Before the first [tab]: file-level settings.
            None if key == "active" => workspace.active = value.parse().with_context(where_)?,
            None if key == "restore-tabs" => {
                workspace.restore_tabs = match value {
                    "yes" => true,
                    "no" => false,
                    _ => anyhow::bail!("{}: invalid restore-tabs value {value:?}", where_()),
                };
            }
            None if key == "fps" => {
                let fps: u32 = value.parse().with_context(where_)?;
                anyhow::ensure!(
                    fps <= MAX_FPS,
                    "{}: fps must be at most {MAX_FPS}",
                    where_()
                );
                workspace.fps = clamp_fps(fps);
            }
            None if key == "idle" => {
                let idle: u32 = value.parse().with_context(where_)?;
                anyhow::ensure!(
                    idle <= MAX_IDLE,
                    "{}: idle must be at most {MAX_IDLE} seconds",
                    where_()
                );
                workspace.idle = idle;
            }
            None if key == "sidebar-width" => {
                let width: u16 = value.parse().with_context(where_)?;
                anyhow::ensure!(
                    (MIN_SIDEBAR_WIDTH..=MAX_SIDEBAR_WIDTH).contains(&width),
                    "{}: sidebar-width must be between {MIN_SIDEBAR_WIDTH} and {MAX_SIDEBAR_WIDTH}",
                    where_()
                );
                workspace.sidebar_width = width;
            }
            None if key == "font-size" => {
                let size: u16 = value.parse().with_context(where_)?;
                anyhow::ensure!(
                    (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size),
                    "{}: font-size must be between {MIN_FONT_SIZE} and {MAX_FONT_SIZE}",
                    where_()
                );
                workspace.font_size = size;
            }
            None if key == "open" => {
                let open: u64 = value.parse().with_context(where_)?;
                anyhow::ensure!(
                    open <= MAX_OPEN_SECS,
                    "{}: open time exceeds {MAX_OPEN_SECS} seconds",
                    where_()
                );
                workspace.open_secs = open;
            }
            None if key == "version" => {
                version = value.parse().with_context(where_)?;
                anyhow::ensure!(
                    matches!(version, 1..=3),
                    "{}: unsupported saved-workspace version {value}",
                    where_()
                );
            }
            None => anyhow::bail!("{}: unexpected key {key} before any [tab]", where_()),
        }
    }
    if let Some(fields) = fields {
        anyhow::ensure!(
            workspace.tabs.len() < MAX_TABS,
            "saved workspace holds more than {MAX_TABS} tabs"
        );
        workspace.tabs.push(tab(&fields, version)?);
    }
    workspace.active = workspace.active.min(workspace.tabs.len().saturating_sub(1));
    Ok(workspace)
}

fn tab(fields: &collections::BTreeMap<&str, &str>, version: u32) -> anyhow::Result<Tab> {
    // Reject unknown keys rather than ignoring them: a file written by a newer
    // Starcom may mean something by them, and guessing is how settings drift.
    for key in fields.keys() {
        anyhow::ensure!(
            matches!(
                *key,
                "destination"
                    | "host"
                    | "user"
                    | "session"
                    | "model"
                    | "window"
                    | "pane"
                    | "port"
                    | "identity"
                    | "known-hosts"
                    | "socket"
                    | "history"
                    | "access"
                    | "reconnect"
            ),
            "unknown key {key}"
        );
    }
    let text = |key: &str| fields.get(key).copied().unwrap_or_default().to_owned();
    let flag = |key: &str, on: &str, off: &str| -> anyhow::Result<bool> {
        match fields.get(key) {
            None => Ok(false),
            Some(value) if *value == on => Ok(true),
            Some(value) if *value == off => Ok(false),
            Some(value) => anyhow::bail!("invalid {key} value {value:?}"),
        }
    };
    let history = fields
        .get("history")
        .map(|history| history.parse())
        .transpose()
        .context("invalid history")?
        .unwrap_or(DEFAULT_HISTORY);
    anyhow::ensure!(
        history <= crate::snapshot::MAX_HISTORY_LINES,
        "history exceeds {} lines",
        crate::snapshot::MAX_HISTORY_LINES
    );
    let id = |key: &str| -> anyhow::Result<Option<u32>> {
        fields
            .get(key)
            .map(|value| value.parse().with_context(|| format!("invalid {key}")))
            .transpose()
    };
    Ok(Tab {
        destination: text("destination"),
        host: text("host"),
        user: text("user"),
        session: text("session"),
        legacy: match fields.get("model") {
            Some(&"window") => false,
            Some(&"legacy-session") => true,
            Some(value) => anyhow::bail!("invalid model value {value:?}"),
            None => version < 3,
        },
        window: id("window")?,
        pane: id("pane")?,
        port: fields
            .get("port")
            .map(|port| port.parse())
            .transpose()
            .context("invalid port")?
            .unwrap_or(22),
        identity: text("identity"),
        known_hosts: text("known-hosts"),
        socket: text("socket"),
        history,
        interactive: flag("access", "interactive", "read-only")?,
        reconnect: flag("reconnect", "yes", "no")?,
    })
}

pub fn render(workspace: &Workspace) -> String {
    let mut out = String::from(
        "# Starcom saved connection tabs.\n\
         # Destinations and preferences only: no keys, passphrases, host-key\n\
         # material, or terminal contents.\n\
         version 3\n",
    );
    out.push_str(&format!("active {}\n", workspace.active));
    out.push_str(&format!(
        "restore-tabs {}\n",
        if workspace.restore_tabs { "yes" } else { "no" }
    ));
    out.push_str(&format!("fps {}\n", clamp_fps(workspace.fps)));
    out.push_str(&format!("idle {}\n", workspace.idle.min(MAX_IDLE)));
    out.push_str(&format!(
        "sidebar-width {}\n",
        clamp_sidebar_width(workspace.sidebar_width)
    ));
    out.push_str(&format!(
        "font-size {}\n",
        clamp_font_size(workspace.font_size)
    ));
    out.push_str(&format!(
        "open {}\n",
        workspace.open_secs.min(MAX_OPEN_SECS)
    ));
    for tab in workspace.tabs.iter().take(MAX_TABS) {
        out.push_str("\n[tab]\n");
        let mut put = |key: &str, value: &str| {
            // A control character could only arrive from a field the user typed;
            // drop the line rather than write something this parser rejects.
            if !value.is_empty() && value.len() <= MAX_VALUE && !value.chars().any(char::is_control)
            {
                out.push_str(key);
                out.push(' ');
                out.push_str(value);
                out.push('\n');
            }
        };
        put("destination", &tab.destination);
        put("host", &tab.host);
        put("user", &tab.user);
        put("session", &tab.session);
        put(
            "model",
            if tab.legacy {
                "legacy-session"
            } else {
                "window"
            },
        );
        if let Some(window) = tab.window {
            put("window", &window.to_string());
        }
        if let Some(pane) = tab.pane {
            put("pane", &pane.to_string());
        }
        put("port", &tab.port.to_string());
        put("identity", &tab.identity);
        put("known-hosts", &tab.known_hosts);
        put("socket", &tab.socket);
        put("history", &tab.history.to_string());
        put(
            "access",
            if tab.interactive {
                "interactive"
            } else {
                "read-only"
            },
        );
        put("reconnect", if tab.reconnect { "yes" } else { "no" });
    }
    out
}

/// Write through a temporary file so an interrupted save cannot leave a
/// half-written workspace in place of the previous one.
pub fn save(file: &path::Path, workspace: &Workspace) -> anyhow::Result<()> {
    let parent = file.parent().context("workspace path has no directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temporary = file.with_extension("conf.new");
    fs::write(&temporary, render(workspace).as_bytes())
        .with_context(|| format!("write {}", temporary.display()))?;
    #[cfg(unix)]
    {
        // Not a secret, but it names the user's hosts and accounts.
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&temporary, file).with_context(|| format!("replace {}", file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Workspace {
        Workspace {
            tabs: vec![
                Tab {
                    destination: "dev".into(),
                    host: "10.0.0.2".into(),
                    user: "alice".into(),
                    session: String::new(),
                    legacy: false,
                    window: Some(4),
                    pane: Some(9),
                    port: 2222,
                    identity: "/home/alice/.ssh/id_ed25519".into(),
                    known_hosts: "/home/alice/.ssh/known_hosts".into(),
                    socket: String::new(),
                    history: 300,
                    interactive: true,
                    reconnect: true,
                },
                Tab {
                    host: "build.example.test".into(),
                    user: "bob".into(),
                    port: 22,
                    history: 200,
                    interactive: false,
                    reconnect: false,
                    ..Tab::default()
                },
            ],
            active: 1,
            restore_tabs: true,
            fps: DEFAULT_FPS,
            idle: DEFAULT_IDLE,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            font_size: DEFAULT_FONT_SIZE,
            open_secs: 0,
        }
    }

    #[test]
    fn fps_is_a_file_level_setting() {
        let mut workspace = sample();
        workspace.fps = 12;
        assert_eq!(parse(&render(&workspace)).unwrap().fps, 12);
        assert_eq!(parse("fps 0\n").unwrap().fps, DEFAULT_FPS);
        assert!(parse("fps 99\n").is_err());
    }

    #[test]
    fn tab_restore_is_enabled_by_default_and_round_trips() {
        assert!(parse("").unwrap().restore_tabs);
        let mut workspace = sample();
        workspace.restore_tabs = false;
        let text = render(&workspace);
        assert!(text.contains("restore-tabs no"));
        assert!(!parse(&text).unwrap().restore_tabs);
        assert!(parse("restore-tabs maybe\n").is_err());
    }

    #[test]
    fn idle_is_a_file_level_setting() {
        let mut workspace = sample();
        workspace.idle = 12;
        assert_eq!(parse(&render(&workspace)).unwrap().idle, 12);
        assert_eq!(parse("idle 0\n").unwrap().idle, 0);
        assert!(parse(&format!("idle {}\n", MAX_IDLE + 1)).is_err());
    }

    #[test]
    fn sidebar_width_is_bounded_and_round_trips() {
        let mut workspace = sample();
        workspace.sidebar_width = 240;
        assert_eq!(parse(&render(&workspace)).unwrap().sidebar_width, 240);
        assert_eq!(parse("").unwrap().sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert!(parse("sidebar-width 119\n").is_err());
        assert!(parse("sidebar-width 321\n").is_err());
    }

    #[test]
    fn font_size_is_bounded_and_round_trips() {
        let mut workspace = sample();
        workspace.font_size = 18;
        assert_eq!(parse(&render(&workspace)).unwrap().font_size, 18);
        assert_eq!(parse("").unwrap().font_size, DEFAULT_FONT_SIZE);
        assert!(parse("font-size 9\n").is_err());
        assert!(parse("font-size 29\n").is_err());
    }

    #[test]
    fn open_time_is_a_file_level_setting() {
        let mut workspace = sample();
        workspace.open_secs = 90;
        assert_eq!(parse(&render(&workspace)).unwrap().open_secs, 90);
        assert_eq!(parse("open 0\n").unwrap().open_secs, 0);
        assert!(parse(&format!("open {}\n", MAX_OPEN_SECS + 1)).is_err());
    }

    #[test]
    fn the_in_repo_example_workspace_parses() {
        let parsed = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/etc/workspace.conf.example"
        )))
        .expect("etc/workspace.conf.example must stay a valid workspace.conf");
        assert!(parsed.restore_tabs);
        assert_eq!(parsed.fps, DEFAULT_FPS);
        assert_eq!(parsed.tabs.len(), 1);
        assert_eq!(parsed.tabs[0].destination, "example");
        assert_eq!(parsed.tabs[0].host, "example.test");
        assert!(parsed.tabs[0].interactive);
        assert!(parsed.tabs[0].reconnect);
        assert!(parsed.tabs[0].session.is_empty());
        assert_eq!(parsed.idle, DEFAULT_IDLE);
    }

    #[test]
    fn tabs_survive_a_round_trip() {
        let workspace = sample();
        let text = render(&workspace);
        assert_eq!(parse(&text).unwrap(), workspace);
    }

    #[test]
    fn last_used_session_is_written() {
        let parsed = parse("version 1\n[tab]\ndestination dev\nsession work\n").unwrap();
        assert_eq!(parsed.tabs[0].session, "work");
        assert_eq!(parsed.tabs[0].window, None);
        assert_eq!(parsed.tabs[0].pane, None);
        assert!(parsed.tabs[0].legacy);
        assert!(render(&parsed).contains("session work"));
        assert!(render(&parsed).contains("version 3"));
        assert!(render(&parsed).contains("model legacy-session"));
    }

    #[test]
    fn v03_tabs_remain_marked_for_the_workspace_to_ignore() {
        let parsed =
            parse("version 2\n[tab]\nhost zork\nuser alice\nsession zork/0\naccess interactive\n")
                .unwrap();
        assert!(parsed.tabs[0].legacy);
        let migrated = render(&parsed);
        assert!(migrated.contains("version 3"));
        assert!(migrated.contains("model legacy-session"));
        assert!(parse(&migrated).unwrap().tabs[0].legacy);
    }

    #[test]
    fn nothing_secret_is_ever_written() {
        let mut workspace = sample();
        // Even if a field somehow held key-shaped text, only known keys are
        // written, and none of them carries key material.
        workspace.tabs[0].session = "work".into();
        // Scan the data, not the explanatory header, which says these words.
        let text: String = render(&workspace)
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        for forbidden in [
            "begin openssh private key",
            "passphrase",
            "password",
            "ssh-rsa aaaa",
            "ssh-ed25519 aaaa",
            "authorized_keys",
            "agent_sock",
        ] {
            assert!(!text.contains(forbidden), "{forbidden} reached the file");
        }
        // The identity is a path to choose again, not the key itself.
        assert!(text.contains("identity /home/alice/.ssh/id_ed25519"));
    }

    #[test]
    fn a_missing_file_is_not_an_error_but_a_malformed_one_is() {
        let directory = scratch();
        let file = directory.0.join("workspace.conf");
        assert!(load(&file).unwrap().is_none());
        save(&file, &sample()).unwrap();
        assert_eq!(load(&file).unwrap().unwrap(), sample());
        fs::write(&file, "[tab]\nport not-a-number\n").unwrap();
        assert!(load(&file).is_err());
        fs::write(&file, "[tab]\nnovel-setting yes\n").unwrap();
        assert!(load(&file).is_err(), "unknown keys must not be ignored");
        fs::write(&file, "version 1\n[tab]\nhost a\nauth agent\n").unwrap();
        assert!(
            load(&file).is_err(),
            "the old agent/key radio must not be guessed at"
        );
        fs::write(&file, "version 4\n").unwrap();
        assert!(
            load(&file).is_err(),
            "a newer format must not be guessed at"
        );
    }

    #[test]
    fn the_file_is_bounded_in_every_direction() {
        assert!(parse(&format!("[tab]\nhost {}\n", "x".repeat(MAX_VALUE + 1))).is_err());
        assert!(parse(&format!("[tab]\nhost {}\n", "x".repeat(MAX_VALUE))).is_ok());
        assert!(parse("[tab]\nhost a\u{1b}b\n").is_err(), "control byte");
        assert!(parse("[tab]\nhost a\n[tab]\nhost b\n").is_ok());
        assert!(
            parse(&format!(
                "[tab]\nhistory {}\n",
                crate::snapshot::MAX_HISTORY_LINES + 1
            ))
            .is_err()
        );
        assert!(parse("[tab]\naccess maybe\n").is_err());
        assert!(parse("[tab]\nreconnect maybe\n").is_err());
        assert!(parse("[tab]\npane not-a-number\n").is_err());
        let many = "[tab]\nhost a\n".repeat(MAX_TABS + 1);
        assert!(parse(&many).is_err(), "tab count must be bounded");
        assert!(parse("[tab]\nhost a\nhost b\n").is_err(), "duplicate key");
        // An out-of-range active index selects a real tab instead of panicking.
        assert_eq!(parse("active 99\n[tab]\nhost a\n").unwrap().active, 0);
        assert_eq!(parse("").unwrap(), Workspace::default());
    }

    #[test]
    fn saving_replaces_the_previous_file_atomically() {
        let directory = scratch();
        let file = directory.0.join("nested").join("workspace.conf");
        save(&file, &sample()).unwrap();
        let mut smaller = sample();
        smaller.tabs.truncate(1);
        smaller.active = 0;
        save(&file, &smaller).unwrap();
        assert_eq!(load(&file).unwrap().unwrap(), smaller);
        assert!(
            !file.with_extension("conf.new").exists(),
            "the temporary file outlived the save"
        );
    }

    struct Scratch(path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> Scratch {
        let path = env::temp_dir().join(format!(
            "starcom-store-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
}
