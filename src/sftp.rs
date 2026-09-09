//! Short-lived SFTP uploads on their own SSH connection.
//!
//! File drops use this instead of the tmux control channel: the control worker
//! stays attached, and a refused or slow transfer cannot stall the session.
//! The protocol is Sunset's sans-io SFTP client driven from the same polling
//! loop as exec.

use std::{fs, io, path, time};

use anyhow::Context;

use crate::ssh;

const MAX_FILES: usize = 8;
/// Files larger than this need an explicit Yes in the status bar.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_NAME: usize = 255;

/// Bytes written so far for the file currently being uploaded.
pub struct Progress<'a> {
    pub name: &'a str,
    pub done: u64,
    pub total: u64,
}

/// Upload local files into the remote temp directory (`realpath("/tmp")`)
/// under unique `starcom-…` names, and return those absolute paths in the
/// same order. `on_progress` is called as each write lands.
pub fn put_files(
    options: &ssh::Options,
    files: &[path::PathBuf],
    on_progress: impl FnMut(Progress<'_>),
) -> anyhow::Result<Vec<String>> {
    put_files_while(options, files, || true, Some(MAX_FILE_BYTES), on_progress)
}

/// Cancellable form used by the desktop. `keep_going` is checked before any
/// connection and between bounded SFTP writes. `max_bytes` is `None` after the
/// user confirmed an oversize drop.
pub(crate) fn put_files_while(
    options: &ssh::Options,
    files: &[path::PathBuf],
    mut keep_going: impl FnMut() -> bool,
    max_bytes: Option<u64>,
    mut on_progress: impl FnMut(Progress<'_>),
) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(!files.is_empty(), "no files to upload");
    anyhow::ensure!(
        files.len() <= MAX_FILES,
        "drop at most {MAX_FILES} files at a time"
    );
    let stamp = time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros())
        .unwrap_or(0);
    let mut prepared = Vec::new();
    for (index, file) in files.iter().enumerate() {
        ensure_running(&mut keep_going)?;
        let name = remote_file_name(file).with_context(|| format!("{}", file.display()))?;
        let remote_name = unique_remote_name(&name, stamp, index)?;
        let meta = fs::metadata(file).with_context(|| format!("stat {}", file.display()))?;
        anyhow::ensure!(
            meta.is_file(),
            "{name} is not a regular file; drop files only"
        );
        ensure_file_size(meta.len(), max_bytes, &name)?;
        prepared.push((file.clone(), remote_name, name, meta.len()));
    }

    let mut options = options.clone();
    options.timeout = options.timeout.max(time::Duration::from_secs(30));
    let deadline = time::Instant::now() + time::Duration::from_secs(300);
    ensure_running(&mut keep_going)?;
    let channel = ssh::Connection::connect(&options)?.subsystem("sftp")?;
    let mut session = Session::new(channel, deadline);

    session
        .sftp
        .init()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    session.send(&[])?;
    match session.event()? {
        Event::Version => {}
        other => anyhow::bail!("SFTP handshake: unexpected {other}"),
    }

    session
        .sftp
        .realpath("/tmp")
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    session.send(&[])?;
    let dir = session.realpath()?;
    anyhow::ensure!(
        dir.starts_with('/') && dir.len() <= 1024 && !dir.chars().any(char::is_control),
        "SFTP temp path is unusable"
    );

    let mut remote = Vec::new();
    let upload = (|| -> anyhow::Result<()> {
        for (file, dest_name, name, total) in prepared {
            ensure_running(&mut keep_going)?;
            let dest = join_remote(&dir, &dest_name);
            anyhow::ensure!(
                dest.len() <= 1024,
                "{name}: remote path exceeds SFTP path limit"
            );
            on_progress(Progress {
                name: &name,
                done: 0,
                total,
            });
            session.put(&file, &dest, total, &mut keep_going, |done| {
                on_progress(Progress {
                    name: &name,
                    done,
                    total,
                });
            })?;
            remote.push(dest);
        }
        Ok(())
    })();
    if let Err(error) = upload {
        // The UI pastes an entire batch or none of it. Best-effort removal
        // avoids leaving earlier files behind when a later file fails.
        for path in remote.iter().rev() {
            let _ = session.remove(path);
        }
        return Err(error);
    }
    Ok(remote)
}

fn ensure_running(keep_going: &mut impl FnMut() -> bool) -> anyhow::Result<()> {
    anyhow::ensure!(keep_going(), "SFTP upload cancelled");
    Ok(())
}

fn ensure_file_size(len: u64, max_bytes: Option<u64>, name: &str) -> anyhow::Result<()> {
    if let Some(max) = max_bytes {
        anyhow::ensure!(
            len <= max,
            "{name} is larger than {} MiB",
            max / (1024 * 1024)
        );
    }
    Ok(())
}

/// Status-bar copy when at least one dropped file is over `MAX_FILE_BYTES`.
pub(crate) fn oversize_notice(files: &[(String, u64)]) -> Option<String> {
    if !files.iter().any(|(_, n)| *n > MAX_FILE_BYTES) {
        return None;
    }
    let limit = MAX_FILE_BYTES / (1024 * 1024);
    if files.len() == 1 {
        let (name, bytes) = &files[0];
        Some(format!(
            "{name} is {:.0} MiB (limit {limit} MiB). Upload anyway?",
            *bytes as f64 / (1024.0 * 1024.0)
        ))
    } else {
        let total: u64 = files.iter().map(|(_, n)| *n).sum();
        Some(format!(
            "{} files, {:.0} MiB total (limit {limit} MiB each). Upload anyway?",
            files.len(),
            total as f64 / (1024.0 * 1024.0)
        ))
    }
}

/// File name used on the host. Rejects path separators so a drop cannot
/// choose a remote directory.
pub(crate) fn remote_file_name(path: &path::Path) -> anyhow::Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("file name is missing or not UTF-8")?;
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\\')
            && !name.chars().any(char::is_control),
        "file name {name:?} cannot be uploaded"
    );
    Ok(name.to_owned())
}

fn unique_remote_name(name: &str, stamp: u128, index: usize) -> anyhow::Result<String> {
    let prefix = format!("starcom-{stamp}-{index}-");
    anyhow::ensure!(
        prefix.len() < MAX_NAME,
        "file name {name:?} cannot be uploaded"
    );
    let mut rest = name.to_owned();
    while prefix.len() + rest.len() > MAX_NAME {
        rest.pop();
    }
    anyhow::ensure!(!rest.is_empty(), "file name {name:?} cannot be uploaded");
    Ok(format!("{prefix}{rest}"))
}

fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn blocked(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

enum Event {
    Version,
    Handle(Vec<u8>),
    Status(sunset_sftp::protocol::StatusCode),
    NameStart(u32),
    Name(Vec<u8>),
    NameEnd,
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version => f.write_str("version"),
            Self::Handle(_) => f.write_str("handle"),
            Self::Status(code) => write!(f, "status {code}"),
            Self::NameStart(_) => f.write_str("name-start"),
            Self::Name(_) => f.write_str("name"),
            Self::NameEnd => f.write_str("name-end"),
        }
    }
}

struct Session {
    channel: ssh::Channel,
    sftp: sunset_sftp::client::SftpRunner<
        { sunset_sftp::client::DEFAULT_CLIENT_BUF },
        { sunset_sftp::client::DEFAULT_CLIENT_BUF },
    >,
    leftover: Vec<u8>,
    leftover_at: usize,
    deadline: time::Instant,
}

impl Session {
    fn new(channel: ssh::Channel, deadline: time::Instant) -> Self {
        Self {
            channel,
            sftp: sunset_sftp::client::SftpRunner::new(),
            leftover: Vec::new(),
            leftover_at: 0,
            deadline,
        }
    }

    fn remaining(&self) -> anyhow::Result<time::Duration> {
        self.deadline
            .checked_duration_since(time::Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| anyhow::anyhow!("SFTP transfer exceeded its deadline"))
    }

    fn send(&mut self, mut data: &[u8]) -> anyhow::Result<()> {
        loop {
            self.remaining()?;
            let mut progressed = false;
            while !self.sftp.output_buf().is_empty() {
                match io::Write::write(&mut self.channel, self.sftp.output_buf()) {
                    Ok(0) => anyhow::bail!("SFTP channel closed during write"),
                    Ok(count) => {
                        self.sftp.consume_output(count);
                        progressed = true;
                    }
                    Err(error) if blocked(&error) => break,
                    Err(error) => return Err(error).context("write SFTP request"),
                }
            }
            if let Some(len) = self.sftp.send_data() {
                anyhow::ensure!(
                    data.len() >= len,
                    "SFTP write payload is shorter than the request"
                );
                match io::Write::write(&mut self.channel, &data[..len]) {
                    Ok(0) => anyhow::bail!("SFTP channel closed during write"),
                    Ok(count) => {
                        self.sftp.data_sent(count);
                        data = &data[count..];
                        progressed = true;
                    }
                    Err(error) if blocked(&error) => {}
                    Err(error) => return Err(error).context("write SFTP payload"),
                }
            }
            if self.sftp.output_done() {
                match io::Write::flush(&mut self.channel) {
                    Ok(()) => return Ok(()),
                    Err(error) if blocked(&error) => {}
                    Err(error) => return Err(error).context("flush SFTP"),
                }
            }
            if !progressed {
                self.channel
                    .wait(self.deadline)
                    .context("wait to send SFTP")?;
            }
        }
    }

    fn event(&mut self) -> anyhow::Result<Event> {
        let mut incoming = [0; 8192];
        while !self.sftp.has_event() {
            self.remaining()?;
            if self.leftover_at < self.leftover.len() {
                let used = self
                    .sftp
                    .input(&self.leftover[self.leftover_at..])
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                self.leftover_at += used;
                if self.leftover_at == self.leftover.len() {
                    self.leftover.clear();
                    self.leftover_at = 0;
                }
                if used == 0 {
                    anyhow::bail!("SFTP parser stalled on leftover input");
                }
                continue;
            }
            match io::Read::read(&mut self.channel, &mut incoming) {
                Ok(0) => anyhow::bail!("SFTP channel ended"),
                Ok(count) => {
                    let mut used = 0;
                    while used < count && !self.sftp.has_event() {
                        let step = self
                            .sftp
                            .input(&incoming[used..count])
                            .map_err(|error| anyhow::anyhow!("{error}"))?;
                        if step == 0 {
                            break;
                        }
                        used += step;
                    }
                    if used < count {
                        self.leftover.extend_from_slice(&incoming[used..count]);
                    }
                }
                Err(error) if blocked(&error) => {
                    self.channel
                        .wait(self.deadline)
                        .context("wait to read SFTP")?;
                }
                Err(error) => return Err(error).context("read SFTP"),
            }
        }
        Ok(
            match self
                .sftp
                .event()
                .ok_or_else(|| anyhow::anyhow!("SFTP event disappeared"))?
            {
                sunset_sftp::client::SftpEvent::Version { .. } => Event::Version,
                sunset_sftp::client::SftpEvent::Handle { handle, .. } => {
                    Event::Handle(handle.to_vec())
                }
                sunset_sftp::client::SftpEvent::Status { code, .. } => Event::Status(code),
                sunset_sftp::client::SftpEvent::NameStart { count, .. } => Event::NameStart(count),
                sunset_sftp::client::SftpEvent::Name { filename, .. } => {
                    Event::Name(filename.to_vec())
                }
                sunset_sftp::client::SftpEvent::NameEnd { .. } => Event::NameEnd,
                other => anyhow::bail!("unexpected SFTP event {other:?}"),
            },
        )
    }

    fn realpath(&mut self) -> anyhow::Result<String> {
        match self.event()? {
            Event::NameStart(count) if count >= 1 => {}
            Event::Status(code) => anyhow::bail!("SFTP realpath failed: {code}"),
            other => anyhow::bail!("SFTP realpath: unexpected {other}"),
        }
        let name = match self.event()? {
            Event::Name(bytes) => String::from_utf8(bytes).context("SFTP realpath is not UTF-8")?,
            other => anyhow::bail!("SFTP realpath: unexpected {other}"),
        };
        match self.event()? {
            Event::NameEnd => Ok(name),
            other => anyhow::bail!("SFTP realpath: unexpected {other}"),
        }
    }

    fn put(
        &mut self,
        local: &path::Path,
        remote: &str,
        expected: u64,
        keep_going: &mut impl FnMut() -> bool,
        mut on_progress: impl FnMut(u64),
    ) -> anyhow::Result<()> {
        use sunset_sftp::client::pflags;
        use sunset_sftp::protocol::Attrs;

        self.sftp
            .open(
                remote,
                // EXCL makes the timestamped name fail safely in the unlikely
                // event of a collision instead of overwriting a remote file.
                pflags::WRITE | pflags::CREAT | pflags::EXCL,
                &Attrs::default(),
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.send(&[])?;
        let handle = match self.event()? {
            Event::Handle(handle) => handle,
            Event::Status(code) => anyhow::bail!("SFTP open {remote}: {code}"),
            other => anyhow::bail!("SFTP open {remote}: unexpected {other}"),
        };

        let result = (|| {
            let mut file =
                fs::File::open(local).with_context(|| format!("open {}", local.display()))?;
            let mut buffer = vec![0; sunset_sftp::client::MAX_WRITE_LEN as usize];
            let mut offset = 0u64;
            loop {
                ensure_running(keep_going)?;
                let count = io::Read::read(&mut file, &mut buffer)
                    .with_context(|| format!("read {}", local.display()))?;
                if count == 0 {
                    break;
                }
                anyhow::ensure!(
                    offset.saturating_add(count as u64) <= expected,
                    "{} changed size while it was being uploaded",
                    local.display()
                );
                self.sftp
                    .write(&handle, offset, count)
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                self.send(&buffer[..count])?;
                match self.event()? {
                    Event::Status(sunset_sftp::protocol::StatusCode::SSH_FX_OK) => {}
                    Event::Status(code) => anyhow::bail!("SFTP write {remote}: {code}"),
                    other => anyhow::bail!("SFTP write {remote}: unexpected {other}"),
                }
                offset += count as u64;
                on_progress(offset);
            }
            anyhow::ensure!(
                offset == expected,
                "{} changed size while it was being uploaded",
                local.display()
            );
            self.sftp
                .close(&handle)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            self.send(&[])?;
            match self.event()? {
                Event::Status(sunset_sftp::protocol::StatusCode::SSH_FX_OK) => Ok(()),
                Event::Status(code) => anyhow::bail!("SFTP close {remote}: {code}"),
                other => anyhow::bail!("SFTP close {remote}: unexpected {other}"),
            }
        })();
        if result.is_err() {
            let _ = self.remove(remote);
        }
        result
    }

    fn remove(&mut self, remote: &str) -> anyhow::Result<()> {
        self.sftp
            .remove(remote)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.send(&[])?;
        match self.event()? {
            Event::Status(sunset_sftp::protocol::StatusCode::SSH_FX_OK) => Ok(()),
            Event::Status(code) => anyhow::bail!("SFTP remove {remote}: {code}"),
            other => anyhow::bail!("SFTP remove {remote}: unexpected {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_reject_path_separators_and_controls() {
        assert_eq!(
            remote_file_name(path::Path::new("/tmp/notes.txt")).unwrap(),
            "notes.txt"
        );
        assert_eq!(
            remote_file_name(path::Path::new("notes.txt")).unwrap(),
            "notes.txt"
        );
        assert!(remote_file_name(path::Path::new(".")).is_err());
        assert!(remote_file_name(path::Path::new("..")).is_err());
        assert!(remote_file_name(path::Path::new("")).is_err());
    }

    #[test]
    fn remote_join_does_not_double_a_trailing_slash() {
        assert_eq!(join_remote("/home/alice", "f"), "/home/alice/f");
        assert_eq!(join_remote("/home/alice/", "f"), "/home/alice/f");
        assert_eq!(join_remote("/tmp", "f"), "/tmp/f");
    }

    #[test]
    fn unique_names_keep_the_original_and_stay_short() {
        let name = unique_remote_name("notes.txt", 1_700_000_000_000_000, 0).unwrap();
        assert_eq!(name, "starcom-1700000000000000-0-notes.txt");
        let long = unique_remote_name(&"a".repeat(MAX_NAME), 0, 0).unwrap();
        assert!(long.len() <= MAX_NAME);
        assert!(long.starts_with("starcom-0-0-"));
        assert!(long.ends_with('a'));
    }

    #[test]
    fn oversize_files_are_named_in_the_confirmation() {
        assert!(oversize_notice(&[("notes.txt".into(), 1024)]).is_none());
        assert_eq!(
            oversize_notice(&[("notes.pdf".into(), 80 * 1024 * 1024)]).as_deref(),
            Some("notes.pdf is 80 MiB (limit 32 MiB). Upload anyway?")
        );
        assert_eq!(
            oversize_notice(&[
                ("a.bin".into(), 40 * 1024 * 1024),
                ("b.bin".into(), 10 * 1024 * 1024)
            ])
            .as_deref(),
            Some("2 files, 50 MiB total (limit 32 MiB each). Upload anyway?")
        );
        assert!(ensure_file_size(MAX_FILE_BYTES, Some(MAX_FILE_BYTES), "a").is_ok());
        assert!(ensure_file_size(MAX_FILE_BYTES + 1, Some(MAX_FILE_BYTES), "a").is_err());
        assert!(ensure_file_size(MAX_FILE_BYTES + 1, None, "a").is_ok());
    }
}
