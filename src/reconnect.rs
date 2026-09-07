//! Reconnection policy: what may be retried, and how long to wait first.
//!
//! There are no sockets, clocks, sleeps, or queued input here. The caller
//! supplies the observed failure and performs the cancellable wait, so every
//! decision is decided by pure code and tested without a network.
//!
//! The classifier is deliberately asymmetric: unrecognized failures are NOT
//! retried. Remote text may only narrow a failure to a non-retriable one, never
//! promote one to retriable, because that text is attacker-influenced output.

use std::time;

#[cfg(feature = "ssh")]
use crate::ssh;

/// Why an attachment ended. Only `Transport` is retried automatically; the rest
/// need a decision Starcom must not make on the user's behalf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// The transport dropped with nothing known to be wrong.
    Transport,
    Authentication,
    HostKey,
    MissingSession,
    /// The remote tmux server exited or restarted underneath the attachment.
    ServerExit,
    /// This control client was detached, here or by another tmux client.
    Detached,
    Configuration,
    Protocol,
}

impl Failure {
    /// Automatic retry is for transport loss alone. Retrying authentication or
    /// host-key failures would be an endless security retry; retrying a missing
    /// session or a detach would fight the user's own decision.
    pub fn retriable(self) -> bool {
        self == Self::Transport
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::Transport => "The connection dropped.",
            Self::Authentication => "Authentication failed. Starcom will not retry it.",
            Self::HostKey => "The host key is not trusted. Starcom will not retry it.",
            Self::MissingSession => {
                "That tmux session no longer exists. Starcom never creates a replacement."
            }
            Self::ServerExit => {
                "The remote tmux server exited. Any session it held is gone; reattaching \
                 would attach to a different server."
            }
            // tmux sends a bare %exit for both an explicit detach and a server
            // shutdown, so do not assert which one happened.
            Self::Detached => {
                "tmux ended this control session. Starcom does not reattach on its own, \
                 because that happens both when you detach and when the server shuts down."
            }
            Self::Configuration => "The connection settings were rejected.",
            Self::Protocol => "The tmux control stream was not usable.",
        }
    }
}

/// Classify a failed attachment or poll.
///
/// Types decide first, because they come from Starcom's own state machines. Only
/// when nothing in the chain is typed does remote text get consulted, and even
/// then it can only make a failure non-retriable.
#[cfg(feature = "ssh")]
pub fn classify(error: &anyhow::Error) -> Failure {
    let typed = error.chain().find_map(|cause| {
        if let Some(error) = cause.downcast_ref::<ssh::Error>() {
            return Some(from_ssh(error.kind));
        }
        let cause = cause.downcast_ref::<std::io::Error>()?;
        // std::io::Error::other keeps its inner error behind get_ref(), not
        // source(), so an SSH error boxed into io never reaches the chain walk.
        if let Some(inner) = cause
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ssh::Error>())
        {
            return Some(from_ssh(inner.kind));
        }
        // The channel surfaces as std::io through Read/Write. An ended channel
        // is transport loss no matter which layer wrapped it.
        matches!(
            cause.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::TimedOut
        )
        .then_some(Failure::Transport)
    });
    match typed {
        // tmux may still have told us the session is gone while the channel was
        // ending. That narrows a retriable failure; it never widens one.
        Some(Failure::Transport) => narrow(&format!("{error:#}")).unwrap_or(Failure::Transport),
        Some(failure) => failure,
        None => narrow(&format!("{error:#}")).unwrap_or(Failure::Protocol),
    }
}

#[cfg(feature = "ssh")]
fn from_ssh(kind: ssh::Kind) -> Failure {
    match kind {
        ssh::Kind::Transport | ssh::Kind::Timeout => Failure::Transport,
        ssh::Kind::Authentication => Failure::Authentication,
        ssh::Kind::UnknownHostKey | ssh::Kind::ChangedHostKey => Failure::HostKey,
        ssh::Kind::Configuration => Failure::Configuration,
    }
}

/// Recognize tmux's own refusals in an error chain. Returns only non-retriable
/// classifications, so a hostile string can suppress a retry but never force one.
fn narrow(text: &str) -> Option<Failure> {
    let text = text.to_ascii_lowercase();
    if text.contains("can't find session")
        || text.contains("session not found")
        || text.contains("no such session")
        // Starcom's own wording for an attach that never attached. tmux reports
        // that inside the control stream before any command is outstanding, so
        // its text does not survive; this phrase does.
        || text.contains("could not attach that session")
    {
        return Some(Failure::MissingSession);
    }
    // tmux words this two ways: "no server running on <default socket>", and
    // "error connecting to <path>" for an explicit -S socket. Both mean the
    // server is gone, and retrying either would loop against nothing.
    if text.contains("no server running")
        || text.contains("server exited")
        || text.contains("error connecting to")
    {
        return Some(Failure::ServerExit);
    }
    None
}

/// Classify tmux's orderly `%exit` reason. tmux sends this when the control
/// client is going away for a reason it already knows.
pub fn classify_exit(reason: Option<&str>) -> Failure {
    let Some(reason) = reason else {
        // A bare %exit is an ordinary detach, not a transport fault.
        return Failure::Detached;
    };
    let reason = reason.to_ascii_lowercase();
    if reason.contains("server exited") || reason.contains("lost server") {
        Failure::ServerExit
    } else if reason.contains("session destroyed") || reason.contains("no such session") {
        Failure::MissingSession
    } else {
        Failure::Detached
    }
}

/// First wait after a drop; short enough that a brief blip is invisible.
pub const FIRST_DELAY: time::Duration = time::Duration::from_millis(500);
/// Ceiling for a single wait. A laptop that slept for an hour still comes back
/// within this bound instead of drifting into a multi-minute backoff.
pub const MAX_DELAY: time::Duration = time::Duration::from_secs(30);
/// Wall time running this far ahead of the monotonic clock is a suspend, not
/// scheduling jitter. NTP steps of a second or two must not force a reconnect.
pub const SUSPEND_SLACK: time::Duration = time::Duration::from_secs(2);

/// True when wall time advanced by more than the monotonic clock, which is what
/// happens across a machine sleep: `Instant` pauses, `SystemTime` does not.
pub fn suspend_elapsed(mono: time::Duration, wall: time::Duration) -> bool {
    wall > mono.saturating_add(SUSPEND_SLACK)
}

/// Last time this attachment did successful I/O, in both clock domains.
#[derive(Clone, Copy, Debug)]
pub struct AliveClock {
    instant: time::Instant,
    wall: time::SystemTime,
}

impl AliveClock {
    pub fn now() -> Self {
        Self {
            instant: time::Instant::now(),
            wall: time::SystemTime::now(),
        }
    }

    pub fn suspended(self) -> bool {
        let mono = time::Instant::now().saturating_duration_since(self.instant);
        let wall = time::SystemTime::now()
            .duration_since(self.wall)
            .unwrap_or(time::Duration::ZERO);
        suspend_elapsed(mono, wall)
    }
}

/// Exponential backoff with deterministic +/-25% jitter.
///
/// It never gives up on its own. A retry loop ends because the user cancelled it
/// or because the failure stopped being retriable — not because a counter ran
/// out, which would silently abandon a session the user still wants.
pub struct Backoff {
    attempt: u32,
    state: u64,
}

impl Backoff {
    /// `seed` only decorrelates the jitter between tabs; it is not a secret and
    /// nothing about the connection may be derived from the delays it produces.
    pub fn new(seed: u64) -> Self {
        Self {
            attempt: 0,
            // Never seed the generator with zero: xorshift would stay there.
            state: seed | 1,
        }
    }

    /// How many attempts have been scheduled, starting at 1 after the first call.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// A successful, fully restored attachment starts the schedule over.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn next_delay(&mut self) -> time::Duration {
        self.attempt = self.attempt.saturating_add(1);
        let steps = self.attempt.saturating_sub(1).min(16);
        let base = FIRST_DELAY
            .saturating_mul(1u32 << steps)
            .min(MAX_DELAY)
            .as_millis() as u64;
        // xorshift64: a few lines of arithmetic instead of a dependency, and
        // reproducible from the seed so the schedule is testable.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        // Uniform over [75%, 125%] of the base delay, then clamped.
        let spread = base / 2;
        let jittered = base.saturating_sub(spread / 2) + (self.state % (spread + 1));
        time::Duration::from_millis(jittered.max(1)).min(MAX_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transport_loss_is_retried_automatically() {
        assert!(Failure::Transport.retriable());
        for failure in [
            Failure::Authentication,
            Failure::HostKey,
            Failure::MissingSession,
            Failure::ServerExit,
            Failure::Detached,
            Failure::Configuration,
            Failure::Protocol,
        ] {
            assert!(!failure.retriable(), "{failure:?} must not auto-retry");
        }
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn transport_errors_are_retriable_and_security_errors_are_not() {
        let cases = [
            (ssh::Kind::Transport, Failure::Transport),
            (ssh::Kind::Timeout, Failure::Transport),
            (ssh::Kind::Authentication, Failure::Authentication),
            (ssh::Kind::UnknownHostKey, Failure::HostKey),
            (ssh::Kind::ChangedHostKey, Failure::HostKey),
            (ssh::Kind::Configuration, Failure::Configuration),
        ];
        for (kind, expected) in cases {
            let error = anyhow::Error::new(ssh::Error::new(kind, "boom"))
                .context("attach the control session");
            assert_eq!(classify(&error), expected, "{kind:?}");
        }
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn remote_text_can_stop_a_retry_but_never_start_one() {
        // tmux refusing the session must not become an endless reattach loop.
        let error = anyhow::Error::new(ssh::Error::new(
            ssh::Kind::Transport,
            "SSH control channel ended",
        ))
        .context("tmux request failed; remote stderr: \"can't find session: work\"");
        assert_eq!(classify(&error), Failure::MissingSession);

        // An explicit -S socket with no server behind it must not retry forever.
        let error = anyhow::Error::new(ssh::Error::new(
            ssh::Kind::Transport,
            "SSH control channel ended",
        ))
        .context(
            "tmux request failed; remote stderr: \"error connecting to /tmp/x.sock \
             (No such file or directory)\"",
        );
        assert_eq!(classify(&error), Failure::ServerExit);

        // The reverse must not hold: remote output claiming a transient fault
        // cannot turn a host-key failure into something Starcom retries.
        let error = anyhow::Error::new(ssh::Error::new(
            ssh::Kind::ChangedHostKey,
            "host key is not trusted",
        ))
        .context("remote said: connection reset, please retry, no server running");
        assert_eq!(classify(&error), Failure::HostKey);

        // An attach that never attached is a missing session, not a protocol
        // fault: tmux ends the control stream and its reason never correlates
        // to one of our commands, so Starcom's own wording is what classifies it.
        let error =
            anyhow::anyhow!("tmux could not attach that session; it may not exist on this server");
        assert_eq!(classify(&error), Failure::MissingSession);

        // An unrecognized failure is never retried.
        assert_eq!(
            classify(&anyhow::anyhow!("something new and unexplained")),
            Failure::Protocol
        );
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn a_channel_that_ended_is_transport_loss_at_any_layer() {
        // The exec channel surfaces through Read/Write, so a drop mid-command
        // arrives as std::io rather than as our own SSH error type.
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::TimedOut,
        ] {
            let error = anyhow::Error::new(std::io::Error::from(kind))
                .context("tmux write failed; delivery is uncertain");
            assert_eq!(classify(&error), Failure::Transport, "{kind:?}");
        }
        // An SSH error nested inside an io error still decides the outcome.
        let nested = anyhow::Error::new(std::io::Error::other(ssh::Error::new(
            ssh::Kind::ChangedHostKey,
            "host key is not trusted",
        )))
        .context("read SSH stdout");
        assert_eq!(classify(&nested), Failure::HostKey);
        // Ordinary io failures are not transport loss.
        assert_eq!(
            classify(&anyhow::Error::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            Failure::Protocol
        );
    }

    #[test]
    fn tmux_exit_reasons_separate_detach_from_server_loss() {
        assert_eq!(classify_exit(None), Failure::Detached);
        assert_eq!(classify_exit(Some("detached")), Failure::Detached);
        assert_eq!(classify_exit(Some("server exited")), Failure::ServerExit);
        assert_eq!(
            classify_exit(Some("server exited unexpectedly")),
            Failure::ServerExit
        );
        assert_eq!(
            classify_exit(Some("session destroyed")),
            Failure::MissingSession
        );
    }

    #[test]
    fn delays_grow_stay_bounded_and_are_jittered_per_connection() {
        let mut backoff = Backoff::new(0x5eed);
        let mut previous = time::Duration::ZERO;
        for attempt in 1..=12 {
            let delay = backoff.next_delay();
            assert_eq!(backoff.attempt(), attempt);
            assert!(delay <= MAX_DELAY, "delay {delay:?} exceeds the ceiling");
            assert!(delay >= time::Duration::from_millis(1));
            if attempt <= 6 {
                assert!(
                    delay >= previous,
                    "attempt {attempt} did not back off: {delay:?} after {previous:?}"
                );
            }
            previous = delay;
        }
        // Late attempts sit at the ceiling rather than growing without bound.
        for _ in 0..8 {
            assert!(backoff.next_delay() <= MAX_DELAY);
        }
        // Two tabs failing at once must not line up on identical schedules.
        let mut a = Backoff::new(1);
        let mut b = Backoff::new(2);
        let left: Vec<_> = (0..6).map(|_| a.next_delay()).collect();
        let right: Vec<_> = (0..6).map(|_| b.next_delay()).collect();
        assert_ne!(left, right, "jitter must decorrelate connections");
        // The same seed reproduces the same schedule, so this stays testable.
        let mut again = Backoff::new(1);
        assert_eq!(left, (0..6).map(|_| again.next_delay()).collect::<Vec<_>>());
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn a_reply_deadline_is_transport_loss_not_a_protocol_fault() {
        // Sleep and NAT black-holes used to surface as the string
        // "tmux reply deadline expired", which classified as Protocol and
        // refused to reconnect. The typed timeout is what retry policy sees.
        let error = anyhow::Error::new(ssh::Error::new(
            ssh::Kind::Timeout,
            "tmux reply deadline expired",
        ));
        assert_eq!(classify(&error), Failure::Transport);
        let wrapped = anyhow::Error::new(ssh::Error::new(
            ssh::Kind::Timeout,
            "tmux write deadline expired; delivery is uncertain",
        ))
        .context("interactive request failed; delivery may be uncertain and was not retried");
        assert_eq!(classify(&wrapped), Failure::Transport);
        // The opaque interactive wrapper must not, by itself, become a retry.
        assert_eq!(
            classify(&anyhow::anyhow!(
                "interactive request failed; delivery may be uncertain and was not retried"
            )),
            Failure::Protocol
        );
    }

    #[test]
    fn wall_time_running_ahead_of_monotonic_is_a_suspend() {
        assert!(!suspend_elapsed(
            time::Duration::from_secs(30),
            time::Duration::from_secs(30)
        ));
        assert!(!suspend_elapsed(
            time::Duration::from_secs(5),
            time::Duration::from_secs(6)
        ));
        assert!(suspend_elapsed(
            time::Duration::from_millis(200),
            time::Duration::from_secs(60)
        ));
        assert!(!AliveClock::now().suspended());
    }

    #[test]
    fn a_restored_attachment_starts_the_schedule_over() {
        let mut backoff = Backoff::new(7);
        for _ in 0..5 {
            backoff.next_delay();
        }
        assert_eq!(backoff.attempt(), 5);
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        assert!(backoff.next_delay() <= FIRST_DELAY.saturating_mul(2));
    }
}
