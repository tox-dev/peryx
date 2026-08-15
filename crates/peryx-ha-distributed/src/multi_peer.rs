//! Fetches due peers in parallel while bounding each peer's backlog. Retry state, validation
//! failures, and terminal retirement remain isolated per peer. Identity-derived jitter spreads retry
//! schedules after shared outages. The caller supplies logical time and drives each round.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::num::NonZeroUsize;
use std::time::Duration;

use futures_util::future::join_all;

use crate::backoff::{ReconnectPolicy, Retry};
use crate::channel::{BoundedChannel, buffer_batch};
use crate::peer::{BatchRequest, PeerTransport, validate_contiguous};
use crate::protocol::Change;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetLimits {
    pub max_concurrent: NonZeroUsize,
    pub request_size: NonZeroUsize,
    /// Bounds retained changes to this value times the roster size.
    pub per_peer_budget: NonZeroUsize,
    /// Zero disables retry jitter.
    pub jitter: Duration,
}

pub const DEFAULT_SET_LIMITS: SetLimits = SetLimits {
    max_concurrent: NonZeroUsize::new(4).expect("4 is non-zero"),
    request_size: NonZeroUsize::new(256).expect("256 is non-zero"),
    per_peer_budget: NonZeroUsize::new(1024).expect("1024 is non-zero"),
    jitter: Duration::from_millis(100),
};

impl Default for SetLimits {
    fn default() -> Self {
        DEFAULT_SET_LIMITS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberOutcome {
    /// `caught_up` indicates that `through` reached the peer's advertised frontier.
    Progressed {
        source: String,
        buffered: usize,
        through: u64,
        caught_up: bool,
    },
    /// The caller must drain and commit before this peer becomes due again.
    BackPressured {
        source: String,
        buffered: usize,
        through: u64,
    },
    /// `delay` is relative to the round's logical `now`.
    RetryAfter { source: String, delay: Duration },
    /// `reason` is the stable machine token for retirement.
    GaveUp { source: String, reason: &'static str },
}

/// Contains due peers in selection order; idle peers are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoundReport {
    pub outcomes: Vec<MemberOutcome>,
}

impl RoundReport {
    /// Never exceeds `max_concurrent`.
    #[must_use]
    pub const fn advanced(&self) -> usize {
        self.outcomes.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Ready,
    BackingOff { until: Duration },
    GaveUp,
}

struct Member<T: PeerTransport> {
    source: String,
    transport: T,
    frontier: u64,
    attempt: u32,
    channel: BoundedChannel,
    health: Health,
    /// Prevents a fetch between drain and commit from replaying drained changes at the old frontier.
    draining: bool,
}

impl<T: PeerTransport> Member<T> {
    fn next_serial(&self) -> u64 {
        self.frontier + self.channel.len() as u64
    }

    fn is_due(&self, now: Duration) -> bool {
        !self.channel.is_full()
            && !self.draining
            && match self.health {
                Health::Ready => true,
                Health::BackingOff { until } => until <= now,
                Health::GaveUp => false,
            }
    }
}

pub struct PeerSet<T: PeerTransport> {
    members: Vec<Member<T>>,
    limits: SetLimits,
    policy: ReconnectPolicy,
    cursor: usize,
    source: Option<String>,
    head: u64,
    version: u16,
}

impl<T: PeerTransport> PeerSet<T> {
    #[must_use]
    pub const fn new(limits: SetLimits, policy: ReconnectPolicy) -> Self {
        Self {
            members: Vec::new(),
            limits,
            policy,
            cursor: 0,
            source: None,
            head: 0,
            version: crate::protocol::PROTOCOL_VERSION,
        }
    }

    /// The driver must propagate this version so apply rejects unsupported peer protocols.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the authoritative writer identity advertised by peers, or `None` before a response.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Retains the highest advertised head across rounds.
    #[must_use]
    pub const fn head(&self) -> u64 {
        self.head
    }

    /// Resumes the peer from its durable `frontier`.
    pub fn join(&mut self, source: impl Into<String>, transport: T, frontier: u64) {
        self.members.push(Member {
            source: source.into(),
            transport,
            frontier,
            attempt: 0,
            channel: BoundedChannel::new(self.limits.per_peer_budget),
            health: Health::Ready,
            draining: false,
        });
    }

    fn member(&self, source: &str) -> Option<&Member<T>> {
        self.members.iter().find(|member| member.source == source)
    }

    fn member_mut(&mut self, source: &str) -> Option<&mut Member<T>> {
        self.members.iter_mut().find(|member| member.source == source)
    }

    #[must_use]
    pub fn frontier(&self, source: &str) -> Option<u64> {
        self.member(source).map(|member| member.frontier)
    }

    #[must_use]
    pub fn buffered(&self, source: &str) -> Option<usize> {
        self.member(source).map(|member| member.channel.len())
    }

    /// Preserves peer join order for deterministic drain and commit.
    #[must_use]
    pub fn sources(&self) -> Vec<String> {
        self.members.iter().map(|member| member.source.clone()).collect()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Returns buffered changes in serial order and suspends fetching until [`commit`](Self::commit).
    /// The hold prevents replay from the unchanged durable frontier.
    pub fn drain(&mut self, source: &str) -> Vec<Change> {
        let Some(member) = self.member_mut(source) else {
            return Vec::new();
        };
        let mut changes = Vec::with_capacity(member.channel.len());
        while let Some(change) = member.channel.pop() {
            changes.push(change);
        }
        member.draining = !changes.is_empty();
        changes
    }

    /// Clears drain and backoff state. Duplicate or stale commits cannot move the frontier backward.
    pub fn commit(&mut self, source: &str, through: u64) {
        if let Some(member) = self.member_mut(source) {
            if through > member.frontier {
                member.frontier = through;
            }
            member.attempt = 0;
            member.health = Health::Ready;
            member.draining = false;
        }
    }

    /// Selects due peers round-robin, fetches up to `max_concurrent` in parallel, and isolates each
    /// peer's backoff or retirement.
    pub async fn advance(&mut self, now: Duration) -> RoundReport {
        let selected = self.select(now);
        if selected.is_empty() {
            return RoundReport::default();
        }
        let fetches = selected.iter().map(|&index| {
            let member = &self.members[index];
            member.transport.fetch_batch(BatchRequest {
                after: member.next_serial(),
                max_operations: self.limits.request_size,
            })
        });
        let results = join_all(fetches).await;
        let mut outcomes = Vec::with_capacity(selected.len());
        for (index, result) in selected.into_iter().zip(results) {
            outcomes.push(self.settle(index, result, now));
        }
        RoundReport { outcomes }
    }

    fn select(&mut self, now: Duration) -> Vec<usize> {
        let count = self.members.len();
        if count == 0 {
            return Vec::new();
        }
        let mut selected = Vec::new();
        for offset in 0..count {
            if selected.len() == self.limits.max_concurrent.get() {
                break;
            }
            let index = (self.cursor + offset) % count;
            if self.members[index].is_due(now) {
                selected.push(index);
            }
        }
        if let Some(&last) = selected.last() {
            self.cursor = (last + 1) % count;
        }
        selected
    }

    fn settle(
        &mut self,
        index: usize,
        result: Result<crate::peer::BatchFrame, crate::peer::TransportError>,
        now: Duration,
    ) -> MemberOutcome {
        let after = self.members[index].next_serial();
        let frame = match result {
            Ok(frame) => frame,
            Err(error) => return self.back_off(index, &error, now),
        };
        // Preserve unsupported versions even when contiguity rejects an empty page first.
        self.version = frame.page().version;
        let (reached, caught_up) = match validate_contiguous(after, frame.page()) {
            Ok(progress) => progress,
            Err(error) => return self.back_off(index, &error, now),
        };
        if !frame.page().source.is_empty() {
            self.source = Some(frame.page().source.clone());
        }
        self.head = self.head.max(frame.page().current_serial);
        let member = &mut self.members[index];
        member.attempt = 0;
        member.health = Health::Ready;
        let outcome = buffer_batch(&mut member.channel, &frame.page().changes);
        let source = member.source.clone();
        if outcome.back_pressure {
            return MemberOutcome::BackPressured {
                source,
                buffered: outcome.accepted,
                through: after + outcome.accepted as u64,
            };
        }
        MemberOutcome::Progressed {
            source,
            buffered: outcome.accepted,
            through: reached,
            caught_up,
        }
    }

    fn back_off(&mut self, index: usize, error: &crate::peer::TransportError, now: Duration) -> MemberOutcome {
        let member = &mut self.members[index];
        member.attempt += 1;
        let source = member.source.clone();
        match self.policy.on_error(error, member.attempt) {
            Retry::After(base) => {
                let delay = base + jitter(&source, member.attempt, self.limits.jitter);
                self.members[index].health = Health::BackingOff { until: now + delay };
                MemberOutcome::RetryAfter { source, delay }
            }
            Retry::GiveUp { reason } => {
                self.members[index].health = Health::GaveUp;
                MemberOutcome::GaveUp { source, reason }
            }
        }
    }
}

/// Derives retry jitter from peer identity and attempt without shared random state.
fn jitter(source: &str, attempt: u32, window: Duration) -> Duration {
    let span = u64::try_from(window.as_nanos()).unwrap_or(u64::MAX);
    if span == 0 {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    attempt.hash(&mut hasher);
    Duration::from_nanos(hasher.finish() % span)
}
