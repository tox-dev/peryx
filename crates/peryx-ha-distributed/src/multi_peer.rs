//! A bounded scheduler that pulls metadata batches from a set of verified peers.
//!
//! [`advance_once`](crate::driver::advance_once) drives one peer; a [`PeerSet`] composes that step
//! across N. One [`advance`](PeerSet::advance) round selects up to `max_concurrent` peers that are
//! due and fetches their batches concurrently, so a slow peer delays only its own progress and never
//! the rest. Each peer buffers into its own [`BoundedChannel`], so no peer's backlog grows past
//! `per_peer_budget` and the set's retained memory stays bounded by the roster size. A peer that hits
//! transport loss backs off on the shared [`ReconnectPolicy`] plus a deterministic, identity-derived
//! jitter, so a shared outage does not resynchronize every replica's retry into a thundering herd. A
//! peer that hits a terminal validation error, or exhausts its retry budget, leaves the rotation
//! without stalling the others.
//!
//! The scheduler holds no clock and spawns nothing: [`advance`](PeerSet::advance) takes the caller's
//! logical `now` and records when a backed-off peer becomes due again, so the whole round is a
//! function of the roster state, the transport results, and that timestamp.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::num::NonZeroUsize;
use std::time::Duration;

use futures_util::future::join_all;

use crate::backoff::{ReconnectPolicy, Retry};
use crate::channel::{BoundedChannel, buffer_batch};
use crate::peer::{BatchRequest, PeerTransport, validate_contiguous};
use crate::protocol::Change;

/// The bounds a [`PeerSet`] applies across its roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetLimits {
    /// The most peers a single round fetches from at once.
    pub max_concurrent: NonZeroUsize,
    /// The operation ceiling on one peer's batch request.
    pub request_size: NonZeroUsize,
    /// The most changes one peer may buffer awaiting apply, bounding the set's retained memory to this
    /// times the roster size.
    pub per_peer_budget: NonZeroUsize,
    /// The width of the deterministic jitter added to a peer's backoff, so peers that fail together do
    /// not retry in lockstep. Zero disables jitter.
    pub jitter: Duration,
}

/// A conservative default: four concurrent peers, 256-operation batches, 1024 buffered changes per
/// peer, and up to 100 ms of anti-herd jitter.
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

/// What one peer's turn in a round produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberOutcome {
    /// The batch buffered in full. `buffered` changes joined the peer's channel, reaching serial
    /// `through`, and `caught_up` marks that the queue met the peer's advertised frontier.
    Progressed {
        source: String,
        buffered: usize,
        through: u64,
        caught_up: bool,
    },
    /// The peer's channel filled before the batch drained. `buffered` changes joined up to `through`;
    /// the caller drains and commits to free slots before the peer is due again.
    BackPressured {
        source: String,
        buffered: usize,
        through: u64,
    },
    /// A retryable transport loss. The peer is due again once the caller's logical clock reaches
    /// `delay` past the round's `now`.
    RetryAfter { source: String, delay: Duration },
    /// A terminal failure or an exhausted retry budget. `reason` is the stable machine token for why
    /// the peer left the rotation.
    GaveUp { source: String, reason: &'static str },
}

/// The peers a round advanced, in selection order. A peer that was not due contributes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoundReport {
    pub outcomes: Vec<MemberOutcome>,
}

impl RoundReport {
    /// How many peers the round fetched from, always at most `max_concurrent`.
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
    /// Set once the peer's buffered changes are drained and cleared once they commit. While it holds,
    /// the peer is not fetched again: a drain empties the channel without advancing the frontier, so
    /// `next_serial` regresses to the frontier and a fetch in that window would re-request the
    /// drained-but-uncommitted changes. Gating the peer here keeps that re-fetch from ever happening,
    /// so the caller's applier need not lean on idempotency to stay correct.
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

/// A bounded, fault-isolating puller over a set of verified metadata peers.
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
    /// Build an empty set with the transfer bounds and the reconnect schedule its peers back off on.
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

    /// The protocol version the peers last advertised. A driver stamps this on the page it applies so the
    /// apply path still rejects a peer speaking an unsupported version, the same as the single-source
    /// sync did.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// The authoritative stream source the peers last advertised, or `None` before any answered. Every
    /// peer serves the same writer's stream, so this is the single identity a follower stamps its own
    /// relayed pages with and a replica applies under.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The highest head serial any peer has advertised, so the caller can report how far the replica lags
    /// the stream even in a round that did not reach it.
    #[must_use]
    pub const fn head(&self) -> u64 {
        self.head
    }

    /// Add a verified peer, resuming from its durably persisted `frontier`.
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

    /// The durable frontier a peer resumes from, or `None` when the set holds no such peer.
    #[must_use]
    pub fn frontier(&self, source: &str) -> Option<u64> {
        self.member(source).map(|member| member.frontier)
    }

    /// How many changes a peer holds awaiting apply, or `None` when the set holds no such peer.
    #[must_use]
    pub fn buffered(&self, source: &str) -> Option<usize> {
        self.member(source).map(|member| member.channel.len())
    }

    /// The source identity of every peer in the set, in join order, so a driver can drain and commit
    /// each after a round.
    #[must_use]
    pub fn sources(&self) -> Vec<String> {
        self.members.iter().map(|member| member.source.clone()).collect()
    }

    /// Whether the set holds no peers, so a caller can tell an unconfigured multi-peer set from one that
    /// is merely idle.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Take every buffered change from a peer, in serial order, for the caller to apply and commit.
    ///
    /// Draining empties the channel without advancing the frontier, so the peer is held out of the fetch
    /// rotation until [`commit`](Self::commit) advances it. Without that hold, a fetch between the drain
    /// and the commit would resume from the regressed frontier and re-request the drained-but-uncommitted
    /// changes; the hold makes that impossible rather than relying on the applier to discard the replay.
    pub fn drain(&mut self, source: &str) -> Vec<Change> {
        let Some(member) = self.member_mut(source) else {
            return Vec::new();
        };
        let mut changes = Vec::with_capacity(member.channel.len());
        while let Some(change) = member.channel.pop() {
            changes.push(change);
        }
        // Hold the peer out of the fetch rotation until its drained changes commit, so a fetch in the
        // window before commit cannot re-request them from the regressed frontier.
        member.draining = !changes.is_empty();
        changes
    }

    /// Advance a peer's durable frontier to `through` after its drained changes are applied and
    /// committed, clearing any backoff so the peer is due again. A `through` at or below the current
    /// frontier leaves it unchanged, so a duplicate commit cannot move a frontier backward.
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

    /// Pull one batch from up to `max_concurrent` peers that are due at `now`, buffering each and
    /// classifying its turn. Peers are chosen round-robin so no peer starves, fetched concurrently so
    /// a slow peer holds up only its own turn, and a failure backs that peer off or retires it without
    /// touching the others.
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
        // Record the advertised version before contiguity validation, so a peer speaking an unsupported
        // version is still surfaced even when it advertises an empty page a validation would reject first.
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

/// A deterministic backoff perturbation in `[0, window)`, derived from the peer identity and attempt
/// so two peers failing on the same attempt draw different delays without any shared random state.
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
