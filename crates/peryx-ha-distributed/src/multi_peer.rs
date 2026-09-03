//! Fetches due peers in parallel while bounding each peer's backlog. Retry state, validation
//! failures, and terminal retirement remain isolated per peer. Identity-derived jitter spreads retry
//! schedules after shared outages. The caller supplies logical time and drives each round.

use std::num::NonZeroUsize;
use std::time::Duration;

use futures_util::future::join_all;

use crate::backoff::{ReconnectPolicy, Retry, jitter};
use crate::channel::{BoundedChannel, buffer_batch};
use crate::peer::{BatchRequest, PeerTransport, TransportError, validate_contiguous};
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
    /// The peer becomes due after `delay` without losing its attempt history.
    Quarantined {
        source: String,
        reason: &'static str,
        delay: Duration,
    },
    /// `reason` is the stable machine token for retirement.
    GaveUp { source: String, reason: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RetiredPeer {
    pub source: String,
    pub reason: &'static str,
}

/// Contains due peers in selection order; idle peers are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoundReport {
    pub outcomes: Vec<MemberOutcome>,
    pub retired: Vec<RetiredPeer>,
    pub fully_retired: bool,
    pub incompatible: Option<u16>,
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
    Quarantined { until: Duration, reason: &'static str },
    Retired { reason: &'static str },
}

struct Member<T: PeerTransport> {
    source: String,
    transport: T,
    frontier: u64,
    attempt: u32,
    channel: BoundedChannel,
    /// Shrinks when a reply overruns the frame bound so the peer is asked for less before it is
    /// given up on, and returns to the roster's size once a page fits.
    request_size: NonZeroUsize,
    batch: Option<BatchIdentity>,
    health: Health,
    /// Prevents a fetch between drain and commit from replaying drained changes at the old frontier.
    draining: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BatchIdentity {
    source: String,
    version: u16,
}

pub struct BufferedBatch {
    pub source: String,
    pub version: u16,
    pub changes: Vec<Change>,
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
                Health::BackingOff { until } | Health::Quarantined { until, .. } => until <= now,
                Health::Retired { .. } => false,
            }
    }

    fn retirement(&self) -> Option<RetiredPeer> {
        let reason = match self.health {
            Health::Quarantined { reason, .. } | Health::Retired { reason } => reason,
            Health::Ready | Health::BackingOff { .. } => return None,
        };
        Some(RetiredPeer {
            source: self.source.clone(),
            reason,
        })
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
            request_size: self.limits.request_size,
            batch: None,
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
        self.drain_batch(source).map_or_else(Vec::new, |batch| batch.changes)
    }

    pub(crate) fn drain_batch(&mut self, source: &str) -> Option<BufferedBatch> {
        let member = self.member_mut(source)?;
        let mut changes = Vec::with_capacity(member.channel.len());
        while let Some(change) = member.channel.pop() {
            changes.push(change);
        }
        member.draining = !changes.is_empty();
        if changes.is_empty() {
            return None;
        }
        let batch = member.batch.take().expect("buffered changes retain their identity");
        Some(BufferedBatch {
            source: batch.source,
            version: batch.version,
            changes,
        })
    }

    /// Duplicate or stale commits cannot move the frontier backward or alter peer health.
    pub fn commit(&mut self, source: &str, through: u64) {
        if let Some(member) = self.member_mut(source) {
            if through > member.frontier {
                member.frontier = through;
            }
            member.draining = false;
        }
    }

    /// The transport a source speaks through, for work that runs beside the change feed rather than in
    /// it. A checkpoint transfer is the one such: the feed refused the cursor, and the recovery reaches
    /// the same peer over the same connection.
    #[must_use]
    pub fn transport(&self, source: &str) -> Option<&T> {
        self.members
            .iter()
            .find(|member| member.source == source)
            .map(|member| &member.transport)
    }

    /// Re-arms a peer retired for a protocol violation. Returns false for an unknown or active peer.
    pub fn rearm(&mut self, source: &str) -> bool {
        let Some(member) = self.member_mut(source) else {
            return false;
        };
        if !matches!(member.health, Health::Retired { .. }) {
            return false;
        }
        member.attempt = 0;
        member.health = Health::Ready;
        true
    }

    pub(crate) fn bind_source(&mut self, source: &str) {
        if self.source.is_none() {
            self.source = Some(source.to_owned());
        }
    }

    /// Selects due peers round-robin, fetches up to `max_concurrent` in parallel, and isolates each
    /// peer's backoff or retirement.
    pub async fn advance(&mut self, now: Duration) -> RoundReport {
        let selected = self.select(now);
        if selected.is_empty() {
            return self.report(Vec::new(), None);
        }
        let fetches = selected.iter().map(|&index| {
            let member = &self.members[index];
            member.transport.fetch_batch(BatchRequest {
                after: member.next_serial(),
                max_operations: member.request_size,
            })
        });
        let results = join_all(fetches).await;
        let mut outcomes = Vec::with_capacity(selected.len());
        let mut incompatible = None;
        for (index, result) in selected.into_iter().zip(results) {
            if let Ok(frame) = &result
                && frame.page().version != crate::protocol::PROTOCOL_VERSION
            {
                incompatible.get_or_insert_with(|| frame.page().version);
            }
            outcomes.push(self.settle(index, result, now));
        }
        self.report(outcomes, incompatible)
    }

    fn report(&self, outcomes: Vec<MemberOutcome>, incompatible: Option<u16>) -> RoundReport {
        let retired: Vec<RetiredPeer> = self.members.iter().filter_map(Member::retirement).collect();
        RoundReport {
            outcomes,
            fully_retired: !self.members.is_empty() && retired.len() == self.members.len(),
            retired,
            incompatible,
        }
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
        result: Result<crate::peer::BatchFrame, TransportError>,
        now: Duration,
    ) -> MemberOutcome {
        let after = self.members[index].next_serial();
        let frame = match result {
            Ok(frame) => frame,
            // A frame the reader could not hold is a request-size problem, not a peer fault, so the
            // peer is asked for fewer records until one record is all that is left to ask for.
            Err(error @ TransportError::FrameTooLarge { .. }) => {
                return self
                    .halve_request(index, now)
                    .unwrap_or_else(|| self.back_off(index, &error, now));
            }
            Err(error) => return self.back_off(index, &error, now),
        };
        let page = frame.page();
        if page.version != crate::protocol::PROTOCOL_VERSION {
            return self.retire(index, "unsupported_version");
        }
        if page.source.is_empty() {
            return self.retire(index, "source_changed");
        }
        if let Some(source) = &self.source
            && source != &page.source
        {
            return self.back_off(
                index,
                &TransportError::SourceChanged {
                    expected: source.clone(),
                    actual: page.source.clone(),
                },
                now,
            );
        }
        let (reached, caught_up) = match validate_contiguous(after, frame.page()) {
            Ok(progress) => progress,
            Err(error) => return self.back_off(index, &error, now),
        };
        self.source.get_or_insert_with(|| page.source.clone());
        self.head = self.head.max(page.current_serial);
        let member = &mut self.members[index];
        member.attempt = 0;
        member.health = Health::Ready;
        member.request_size = self.limits.request_size;
        let outcome = buffer_batch(&mut member.channel, &page.changes);
        if outcome.accepted > 0 && member.batch.is_none() {
            member.batch = Some(BatchIdentity {
                source: page.source.clone(),
                version: page.version,
            });
        }
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

    fn retire(&mut self, index: usize, reason: &'static str) -> MemberOutcome {
        let member = &mut self.members[index];
        member.health = Health::Retired { reason };
        MemberOutcome::GaveUp {
            source: member.source.clone(),
            reason,
        }
    }

    /// Halves this peer's next request, or returns `None` when it already asks for one record.
    fn halve_request(&mut self, index: usize, now: Duration) -> Option<MemberOutcome> {
        let member = &mut self.members[index];
        let smaller = NonZeroUsize::new(member.request_size.get() / 2)?;
        member.request_size = smaller;
        let source = member.source.clone();
        let delay = jitter(&source, member.attempt, self.limits.jitter);
        member.health = Health::BackingOff { until: now + delay };
        Some(MemberOutcome::RetryAfter { source, delay })
    }

    fn back_off(&mut self, index: usize, error: &TransportError, now: Duration) -> MemberOutcome {
        let member = &mut self.members[index];
        member.attempt = member.attempt.saturating_add(1);
        let source = member.source.clone();
        match self.policy.on_error(error, member.attempt) {
            Retry::After(base) => {
                let delay = base + jitter(&source, member.attempt, self.limits.jitter);
                self.members[index].health = Health::BackingOff { until: now + delay };
                MemberOutcome::RetryAfter { source, delay }
            }
            Retry::GiveUp { reason } => {
                if error.requires_explicit_rearm() {
                    self.members[index].health = Health::Retired { reason };
                    MemberOutcome::GaveUp { source, reason }
                } else {
                    let delay = self.policy.quarantine_delay() + jitter(&source, member.attempt, self.limits.jitter);
                    self.members[index].health = Health::Quarantined {
                        until: now + delay,
                        reason,
                    };
                    MemberOutcome::Quarantined { source, reason, delay }
                }
            }
        }
    }
}
