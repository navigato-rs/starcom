//! Bounded framing and lifecycle around tmuxctl's I/O-independent engine.
//!
//! We use `Engine::on_line`, not its unbounded byte buffer. The small guard
//! validator also rejects mismatched/nested reply blocks before the upstream
//! parser can accept them. Notifications and terminal escaping remain tmuxctl's job.

use std::fmt;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub line_bytes: usize,
    pub reply_bytes: usize,
    pub reply_lines: usize,
    pub pending_commands: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            line_bytes: 64 * 1024,
            reply_bytes: 1024 * 1024,
            reply_lines: 16 * 1024,
            pending_commands: 128,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidLimits,
    Closed,
    LineTooLong,
    ReplyTooLarge,
    TooManyReplyLines,
    TooManyPendingCommands,
    MalformedGuard,
    UnexpectedGuard,
    MismatchedGuard,
    NestedReply,
    OutOfOrderReply,
    LayoutTooDeep,
    TruncatedStream,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tmux control stream: {self:?}")
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Guard {
    timestamp: u64,
    number: u64,
    flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardKind {
    Begin,
    End,
    Error,
}

struct ReplyBudget {
    guard: Guard,
    bytes: usize,
    lines: usize,
}

pub struct Control {
    engine: tmuxctl::Engine,
    limits: Limits,
    line: Vec<u8>,
    reply: Option<ReplyBudget>,
    last_control_number: Option<u64>,
    pending: usize,
    closed: bool,
}

impl Default for Control {
    fn default() -> Self {
        Self::new(Limits::default()).expect("default protocol limits are valid")
    }
}

impl Control {
    pub fn new(limits: Limits) -> Result<Self, Error> {
        if limits.line_bytes == 0
            || limits.reply_bytes == 0
            || limits.reply_lines == 0
            || limits.pending_commands == 0
        {
            return Err(Error::InvalidLimits);
        }
        Ok(Self {
            engine: tmuxctl::Engine::new(),
            limits,
            line: Vec::new(),
            reply: None,
            last_control_number: None,
            pending: 0,
            closed: false,
        })
    }

    /// Register immediately before a serialized write. On any partial/failed
    /// write, close the channel and call `finish`; never resend the command.
    pub fn register_command(&mut self) -> Result<tmuxctl::CommandId, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.pending >= self.limits.pending_commands {
            return Err(Error::TooManyPendingCommands);
        }
        self.pending += 1;
        Ok(self.engine.register_command())
    }

    pub fn pending_commands(&self) -> usize {
        self.pending
    }

    /// Emit incrementally, so an arbitrarily large input chunk does not create
    /// an arbitrarily large vector of notifications inside this adapter.
    /// On failure all pending commands are emitted as disconnected outcomes.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(tmuxctl::Incoming),
    ) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        for &byte in bytes {
            if self.closed {
                return Err(Error::Closed);
            }
            if byte != b'\n' {
                if self.line.len() >= self.limits.line_bytes {
                    self.close(&mut emit);
                    return Err(Error::LineTooLong);
                }
                self.line.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let outcome = self.process_line(&line);
            self.line = line;
            self.line.clear();
            match outcome {
                Ok(Some(event)) => {
                    let exit = matches!(
                        event,
                        tmuxctl::Incoming::Notification(tmuxctl::Notification::Exit(_))
                    );
                    emit(event);
                    if exit {
                        self.close(&mut emit);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.close(&mut emit);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// EOF is not an acknowledgment. Resolve outstanding commands even when
    /// the stream ends in the middle of a line or reply block.
    pub fn finish(&mut self, mut emit: impl FnMut(tmuxctl::Incoming)) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        let truncated = !self.line.is_empty() || self.reply.is_some();
        self.close(&mut emit);
        if truncated {
            Err(Error::TruncatedStream)
        } else {
            Ok(())
        }
    }

    fn close(&mut self, emit: &mut impl FnMut(tmuxctl::Incoming)) {
        if self.closed {
            return;
        }
        self.closed = true;
        for event in self.engine.on_eof() {
            emit(event);
        }
        self.engine = tmuxctl::Engine::new();
        self.line.clear();
        self.reply = None;
        self.pending = 0;
    }

    fn process_line(&mut self, line: &[u8]) -> Result<Option<tmuxctl::Incoming>, Error> {
        let envelope = parse_guard(line)?;
        match self.reply {
            Some(ref mut reply) => match envelope {
                Some((GuardKind::Begin, _)) => return Err(Error::NestedReply),
                Some((_, guard)) => {
                    if guard != reply.guard {
                        return Err(Error::MismatchedGuard);
                    }
                    if guard.flags != 0 {
                        self.last_control_number = Some(guard.number);
                    }
                    self.reply = None;
                }
                None => {
                    reply.bytes = reply
                        .bytes
                        .checked_add(line.len() + 1)
                        .ok_or(Error::ReplyTooLarge)?;
                    reply.lines += 1;
                    if reply.bytes > self.limits.reply_bytes {
                        return Err(Error::ReplyTooLarge);
                    }
                    if reply.lines > self.limits.reply_lines {
                        return Err(Error::TooManyReplyLines);
                    }
                }
            },
            None => match envelope {
                Some((GuardKind::Begin, guard)) => {
                    if guard.flags != 0
                        && self
                            .last_control_number
                            .is_some_and(|last| guard.number <= last)
                    {
                        return Err(Error::OutOfOrderReply);
                    }
                    self.reply = Some(ReplyBudget {
                        guard,
                        bytes: 0,
                        lines: 0,
                    });
                }
                Some(_) => return Err(Error::UnexpectedGuard),
                None => check_layout_depth(line)?,
            },
        }
        let event = self.engine.on_line(line);
        if matches!(event, Some(tmuxctl::Incoming::Reply { .. })) {
            self.pending -= 1;
        }
        Ok(event)
    }
}

fn parse_guard(line: &[u8]) -> Result<Option<(GuardKind, Guard)>, Error> {
    let kind = if line.starts_with(b"%begin ") || line == b"%begin" {
        GuardKind::Begin
    } else if line.starts_with(b"%end ") || line == b"%end" {
        GuardKind::End
    } else if line.starts_with(b"%error ") || line == b"%error" {
        GuardKind::Error
    } else {
        return Ok(None);
    };
    let text = std::str::from_utf8(line).map_err(|_| Error::MalformedGuard)?;
    let mut fields = text.split(' ');
    fields.next();
    let timestamp = fields
        .next()
        .ok_or(Error::MalformedGuard)?
        .parse()
        .map_err(|_| Error::MalformedGuard)?;
    let number = fields
        .next()
        .ok_or(Error::MalformedGuard)?
        .parse()
        .map_err(|_| Error::MalformedGuard)?;
    let flags = fields
        .next()
        .ok_or(Error::MalformedGuard)?
        .parse()
        .map_err(|_| Error::MalformedGuard)?;
    if fields.next().is_some() {
        return Err(Error::MalformedGuard);
    }
    Ok(Some((
        kind,
        Guard {
            timestamp,
            number,
            flags,
        },
    )))
}

fn check_layout_depth(line: &[u8]) -> Result<(), Error> {
    if !line.starts_with(b"%layout-change ") {
        return Ok(());
    }
    let mut depth = 0usize;
    for &byte in line {
        match byte {
            b'{' | b'[' => {
                depth += 1;
                if depth > 64 {
                    return Err(Error::LayoutTooDeep);
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_chunks_preserve_non_utf8_pane_output() {
        let wire = b"%output %3 a\\033[31m\xff\\012\n";
        for split in 0..=wire.len() {
            let mut control = Control::default();
            let mut events = Vec::new();
            control
                .feed(&wire[..split], |event| events.push(event))
                .unwrap();
            control
                .feed(&wire[split..], |event| events.push(event))
                .unwrap();
            assert_eq!(
                events,
                vec![tmuxctl::Incoming::Notification(
                    tmuxctl::Notification::Output {
                        pane: tmuxctl::PaneId(3),
                        bytes: b"a\x1b[31m\xff\n".to_vec(),
                    }
                )]
            );
        }
    }

    #[test]
    fn startup_reply_does_not_consume_a_registered_command() {
        let mut control = Control::default();
        let id = control.register_command().unwrap();
        let mut events = Vec::new();
        control
            .feed(
                b"%begin 1 1 0\n%end 1 1 0\n%begin 1 2 1\na\n\nb\n%end 1 2 1\n",
                |event| events.push(event),
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        match events[0] {
            tmuxctl::Incoming::Reply {
                id: got,
                result: Ok(ref output),
            } => {
                assert_eq!(got, id);
                assert_eq!(output.lines, ["a", "", "b"]);
            }
            ref other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(control.pending_commands(), 0);
    }

    #[test]
    fn several_registered_commands_complete_in_wire_order() {
        let mut control = Control::default();
        let ids = [
            control.register_command().unwrap(),
            control.register_command().unwrap(),
            control.register_command().unwrap(),
        ];
        assert_eq!(control.pending_commands(), ids.len());
        let mut events = Vec::new();
        control
            .feed(
                b"%begin 1 2 1\none\n%end 1 2 1\n%output %3 x\n%begin 1 3 1\ntwo\n%end 1 3 1\n%begin 1 4 1\nthree\n%end 1 4 1\n",
                |event| events.push(event),
            )
            .unwrap();
        let completed: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                tmuxctl::Incoming::Reply { id, .. } => Some(*id),
                tmuxctl::Incoming::Notification(_) => None,
            })
            .collect();
        assert_eq!(completed, ids);
        assert_eq!(control.pending_commands(), 0);
    }

    #[test]
    fn malformed_reply_closes_and_fails_pending_commands() {
        let mut control = Control::default();
        let id = control.register_command().unwrap();
        let mut events = Vec::new();
        let error = control.feed(b"%begin 1 2 1\n%end 1 99 1\n", |e| events.push(e));
        assert_eq!(error, Err(Error::MismatchedGuard));
        assert_eq!(
            events,
            vec![tmuxctl::Incoming::Reply {
                id,
                result: Err(tmuxctl::CommandError::Disconnected),
            }]
        );
        assert_eq!(control.register_command(), Err(Error::Closed));
    }

    #[test]
    fn truncated_eof_resolves_commands() {
        let mut control = Control::default();
        control.register_command().unwrap();
        control.feed(b"%begin 1 2 1\npartial", |_| {}).unwrap();
        let mut events = Vec::new();
        assert_eq!(
            control.finish(|e| events.push(e)),
            Err(Error::TruncatedStream)
        );
        assert_eq!(events.len(), 1);
        assert!(control.finish(|_| panic!("duplicate completion")).is_ok());
    }

    #[test]
    fn limits_apply_before_upstream_buffering() {
        let limits = Limits {
            line_bytes: 16,
            reply_bytes: 4,
            pending_commands: 1,
            ..Limits::default()
        };
        let mut control = Control::new(limits).unwrap();
        control.register_command().unwrap();
        assert_eq!(
            control.register_command(),
            Err(Error::TooManyPendingCommands)
        );
        assert_eq!(control.feed(&[b'x'; 17], |_| {}), Err(Error::LineTooLong));
        let mut control = Control::new(limits).unwrap();
        assert_eq!(
            control.feed(b"%begin 1 1 0\n12345\n", |_| {}),
            Err(Error::ReplyTooLarge)
        );
    }

    #[test]
    fn explicit_exit_drains_pending_without_replay() {
        let mut control = Control::default();
        control.register_command().unwrap();
        let mut events = Vec::new();
        control
            .feed(b"%exit detached\n", |e| events.push(e))
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(control.pending_commands(), 0);
        assert_eq!(
            control.feed(b"%sessions-changed\n", |_| {}),
            Err(Error::Closed)
        );
    }

    #[test]
    fn nested_and_reordered_replies_are_errors_not_panics() {
        let mut control = Control::default();
        assert_eq!(
            control.feed(b"%begin 1 1 0\n%begin 1 2 0\n", |_| {}),
            Err(Error::NestedReply)
        );
        let mut control = Control::default();
        control.feed(b"%begin 1 2 1\n%end 1 2 1\n", |_| {}).unwrap();
        assert_eq!(
            control.feed(b"%begin 1 2 1\n", |_| {}),
            Err(Error::OutOfOrderReply)
        );
    }
}
