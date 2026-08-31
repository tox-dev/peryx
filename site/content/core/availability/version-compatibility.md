+++
title = "Version compatibility and rolling upgrades"
description = "Record the design for mixed-version negotiation and rollback limits."
weight = 9
aliases = [ "/core/availability-version-compatibility/"]
+++

The negotiation, command barrier, rollout preflight, and irreversible-migration floor on this page are design-only.
Their types and tests ship, but no startup, command, or HTTP path calls them. Shipped replication rejects an
incompatible schema through `GET /+replication/v1/ready`; operators must manage compatible upgrades themselves.

Rolling an availability cluster through an upgrade mixes nodes at different builds. Before two nodes exchange commands
they must agree on versions both speak, hold a new command until every committed member can apply it, and refuse an
operation an older member could misread. This contract complements the
[availability contracts](@/core/availability/contracts.md) that define durability for each mutation mode. It governs the
same [authority transfers](@/core/availability/authority-transfer.md) that move a home between datacenters, since a
transfer must not commit a version a survivor cannot run.

The design places negotiation during distributed preparation and would stop activation on a mismatch. The current
service assembly does not perform that negotiation.

Two dimensions carry a version, each a `u16` where higher is newer: the **wire protocol** two nodes speak on the
replication channel, and the **replicated state machine** that applies committed operations. A node advertises an
inclusive `min..=max` range per dimension and speaks every version in it, so a build keeps its oldest still-supported
version reachable rather than jumping its floor forward on every release.

## The compatibility matrix

Two nodes interoperate on a dimension when their advertised ranges overlap, and they settle on the highest version both
support so an upgraded pair uses its newest common capability. Negotiation resolves the protocol dimension first, then
the state machine; a dimension whose ranges do not overlap makes the pair incompatible and names which dimension failed.

The matrix below reads a local node advertising protocol `2..=4` against peers at a range of builds:

| Peer protocol range | Overlap | Negotiated protocol | Outcome                       |
| ------------------- | ------- | ------------------- | ----------------------------- |
| `1..=2`             | `2`     | `2`                 | interoperate at the floor     |
| `3..=5`             | `3..=4` | `4`                 | interoperate at the ceiling   |
| `2..=4`             | `2..=4` | `4`                 | identical builds, newest wins |
| `5..=6`             | none    | -                   | incompatible on protocol      |

The state-machine dimension negotiates by the same rule. An operator upgrades safely by keeping each new build's range
overlapping the range of every node still in the cluster, one hop at a time, so no negotiation ever falls through.

## The command barrier

A newer state-machine version may introduce a command an older member cannot apply. Issuing it while any committed
member is still behind would leave that member unable to replay the log, so a feature introduced at a state-machine
version stays disabled until every committed member advertises support for it. An empty membership activates nothing, so
a cluster that has not yet confirmed its voters never issues a version-gated command. The barrier lifts only once the
whole committed set has upgraded past the feature's floor, which is why a rolling upgrade enables new behavior at the
end of the roll rather than as each node restarts.

## Rejected versions and operations

A receiver fails closed on anything it cannot safely interpret. An operation kind arrives with a stable wire
discriminant and a flag for whether a receiver must understand it: a known discriminant always applies, and an unknown
one applies only when the sender marked it ignorable. An unknown **required** kind is rejected rather than skipped,
because applying the surrounding log while dropping a required operation would diverge the receiver's state. Wire
discriminants are stable across versions for this reason: a discriminant never means one thing to one build and another
to the next.

Version skips follow the same fail-closed rule. Nodes move one negotiated version at a time; a jump past a version no
peer still advertises has no shared step to land on and is refused at negotiation rather than guessed at.

## The rollback boundary

A downgrade is a negotiation like any other until a migration makes it irreversible. When the state machine crosses a
version that rewrites persisted state in a form an older build cannot read, a snapshot written past that point can no
longer be restored by that build, so the cluster records the crossed version as an irreversible-migration floor. A
rollback below the floor would strand every node that already took a post-migration snapshot.

Before a rolling upgrade commits a new operating version, a preflight decides whether the target clears the version
rules. Every committed voter must already run the target on both dimensions, so a command at the new version never
reaches a member that cannot apply it, and the target state machine must sit at or above the irreversible-migration
floor. A preflight reports an unsupported target, a rollback below the floor, or both in a fixed order, so the verdict
is deterministic and an operator sees every reason to wait at once. The operational side of the same decision. Quorum,
replication lag, and backup currency come from the datacenter group's readiness, not from the version contract.
