+++
title = "Visibility replication"
description = "How trash, restore, revoke, and lift transitions replicate: the ordering that keeps them safe, the tombstones a replica retains, and what dc and ha promise on failover."
weight = 12
+++

Trash, restore, revoke, and lift are the four transitions that decide whether a replica may serve an artifact. Unlike an
upload, a visibility transition can be _undone_ and _redone_, and a replica that applies them out of order can land on
the wrong answer: a revoked artifact served again, a restored one still hidden. This page describes how those
transitions replicate, the ordering that makes duplicate and reordered delivery safe, and the consistency, lag, and
recovery each mutation mode promises. It refines the [availability contracts](@/core/availability-contracts.md) for the
visibility projection specifically; the read-side frontier it gates on is the one the
[derived-view frontiers](@/core/availability-derived-views.md) page defines.

## The operation and its order

A transition is a typed **visibility operation**: which artifact it targets, which of the four transitions it is, and
where it sits in the authoritative order. The order is a pair : the **authority epoch** first, the operation **serial**
within that epoch : and the authority that owns the artifact stamps it. A higher epoch always outranks a lower one; a
higher serial outranks a lower one within an epoch. The serial is drawn from the same monotonic metadata journal counter
every other mutation uses, so a visibility operation orders against uploads and yanks by the one serial the contract
already expresses staleness over.

The operation rides the existing primary-to-replica change feed, not a side channel. It becomes a change whose serial is
the operation's own serial, wrapped in the replication envelope tagged as a visibility operation. A replica routes a
visibility envelope to its visibility projection and applies every other kind as before, so the transition inherits the
feed's ordering, back-pressure, and recovery rather than reinventing them.

{% mermaid() %}
flowchart LR
auth["home authority"] -->|mint epoch and serial| feed["change feed"]
feed -->|visibility envelope| proj["replica projection"]
proj -->|apply then persist| snap["visibility snapshot"]
proj -->|only then advertise| front["operation frontier"]
front -->|gates| view["served protocol view"]
class auth,feed accent
class proj,snap good
class front,view warn
{% end %}

## Why ordering is safe

The projection is idempotent and monotonic per dimension. Trash/restore and revoke/lift are independent dimensions, each
with its own high-water order. Applying an operation advances a dimension only when its order is newer than that
dimension's last; a duplicate or an older, reordered operation leaves the state unchanged. A revoke a replica has
already applied is not undone by a lift that was authored earlier and merely arrived late, so **duplicate and
out-of-order delivery cannot resurrect an older state**. Because the two dimensions are independent, a trash and a
revoke on one artifact do not fence each other out by serial.

A replica advertises an **operation frontier** : the highest serial per epoch whose applied effect it has durably
persisted. The projection persists the converged snapshot _before_ it advances that frontier, and a batch commits its
whole effect or none of it: if the persist fails, the projection and its advertised frontier stay exactly where they
were. A reader that trusts the frontier can therefore trust the served projection behind it, and a replica never
advertises coverage of a transition it has not durably applied.

## Tombstones and compaction

A hidden artifact is held out of sight by a **tombstone**: the retained record that it is trashed or revoked. A replica
keeps every tombstone in a durable visibility snapshot, so a restart or a metadata log compaction recovers the full
hidden set rather than silently resurrecting an artifact whose tombstone was dropped. The snapshot fails closed: a build
that cannot restore it refuses to start rather than serve a partial hidden set.

Retention is bounded by frontier, never by wall-clock age. Compaction releases an artifact only once it has returned to
the visible default _and_ a required-replica-and-backup frontier covers its operations, because the authority never
resends an operation below a serial acknowledged everywhere. A still-trashed or still-revoked artifact is never released
\: its tombstone is the thing doing the hiding. An entry whose high-water sits in a later epoch is kept until every
earlier epoch has drained too, so a stale lower-epoch operation cannot resurrect an entry the compaction has forgotten.

## Failover

A failover advances the authority epoch. Because the order compares epoch first, every operation the new home mints
outranks every operation the prior home produced, whatever its serial. A late-arriving operation from the old epoch :
still in flight when the transfer completed : is applied against the new epoch's high-water and dropped as stale. So
**failover preserves the delete, restore, revoke, and lift ordering**: the transition the new authority intends wins,
and the old home cannot pull a dimension back to a value it already lost. The epoch a home stamps only ever moves
upward, which is what fences a stale home that rejoins.

## Consistency, lag, and recovery by mode

Visibility state is metadata: the small authoritative record and the serial that orders it, not artifact bytes. A
tombstone hides bytes; it never moves them. So a visibility transition carries the **metadata** promise of its mutation
mode, and never waits on byte convergence.

- **`none`** applies the transition locally on the single writer and streams it to read replicas over the feed. There is
  no second synchronous copy: the worst-case loss on writer failure is every transition committed since the last backup
  you can verify, and recovery is the writer restore or promotion the
  [failover and recovery](@/core/availability-failover-recovery.md) guide describes. A replica lags the writer by its
  own catch-up; it hides the lag by serving only up to its readable frontier.
- **`dc`** makes the transition durable in a second failure domain within the datacenter before it acknowledges. It
  survives the loss of one node's storage with no lost transition; it does not survive the loss of the datacenter.
- **`ha`** makes the transition durable in a remote datacenter before it acknowledges. A trash, restore, revoke, or lift
  survives the loss of the writing datacenter, and a reader in the surviving datacenter serves the same hidden set the
  transition intended.

**Consistency across modes is the same:** a replica exposes the projection only up to its readable frontier, and it
advertises the operation frontier only once the projection behind it is durable. A reader never sees a transition half
applied : new metadata paired to an old visibility view : regardless of mode. The modes differ only in how much is at
risk when a failure domain is lost, not in what a served answer means.

**Lag signals.** Each replica's advertised operation frontier, read against the home's current serial, is the lag of its
visibility projection; a replica that has caught up serves the transitions the home has committed. The
[readiness probe](@/core/high-availability.md#availability-health-and-readiness) reports whether a replica's derived
views : the visibility projection among them : have reached the frontier it advertises. Compaction that stalls is itself
a signal: a required replica or backup that has not acknowledged an epoch holds tombstones from being released, so a
growing retained set points at a lagging or absent member rather than at the compaction.

**Recovery objectives.** Because a visibility transition never gates on bytes, its recovery objectives are the metadata
objectives of its mode:

| Mode   | Visibility RPO on failure-domain loss                    | Visibility RTO                                            |
| ------ | -------------------------------------------------------- | --------------------------------------------------------- |
| `none` | Every transition since the last verifiable backup        | Writer restore or manual promotion, then replica catch-up |
| `dc`   | Zero within the datacenter; DC loss falls back to `none` | In-DC promotion of the synchronous copy, then catch-up    |
| `ha`   | Zero across the loss of the writing datacenter           | Cross-DC failover to the remote copy, then catch-up       |

A recovered replica restores its visibility snapshot : every retained tombstone : and resumes applying from its durable
cursor, so recovery reproduces the exact hidden set the member last persisted before it advertises a frontier again.
