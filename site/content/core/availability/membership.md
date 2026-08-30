+++
title = "Managing voting membership"
description = "Describe HA membership commands and their deployment boundary."
weight = 9
aliases = [ "/core/availability-membership/"]
+++

HA command handlers and Raft membership storage ship, but public peer routes and the private Raft listener cannot share
the configured member address. Mode `dc` uses its static roster and runs no ownership consensus.

An HA ownership consensus group has a voting roster: the datacenters whose acknowledgement a committed authority change
needs. An administrator changes that roster through the
[availability control listener](@/core/availability/listener.md), never by editing a store. Every change commits as a
Raft membership entry, so the group applies it in one order and a restart recovers it.

Peryx selects voters and applies promotion policy. Storage persists committed entries without interpreting them.

## Onboarding and promotion

A new datacenter joins as a **learner** first. A learner replicates the log and catches up to the leader's frontier
without counting toward quorum, so onboarding one never risks the group's availability. Once the learner is caught up,
the administrator **promotes** it to a voter, and from then on its acknowledgement counts.

Splitting the join in two keeps a lagging newcomer out of the quorum math. A learner that is still copying the backlog
cannot stall a commit, because the group's quorum is still the existing voters; only the promotion, which the
administrator issues when the learner is ready, admits it to the roster.

## Replacement and removal

**Replacing** a voter swaps one datacenter for another in a single roster rewrite: the incoming datacenter is added as a
learner and swapped in for the outgoing one. **Removing** a voter drops it from the roster outright. A rewrite that
leaves the voter set unchanged, promoting a datacenter that already votes or removing one that is absent, commits as a
no-op rather than a distinct entry, so a retried command is idempotent.

Liveness suspicion never changes the roster. A voter the group cannot currently reach stays a voter; only an explicit
administrator command adds or removes one, so a transient partition never silently reshapes the quorum.

## What a change records

Each committed membership command retains an audit line naming the actor who submitted it, the command and its target
datacenter, the result, the committed term and index, and the **old and new voter sets** the change moved the group
between. The audit never carries the request body, so a peer address or a credential never reaches the log. The old and
new sets make the roster transition auditable after the fact: a reviewer sees exactly which datacenters voted before and
after each change.

## Authorization

The control listener authenticates every request against the same identity store the package API uses. A membership
mutation requires the server-wide `AdministrationWrite` scope, and reading the roster or cluster status requires
`AdministrationRead`. A principal without the scope its operation needs is denied before the command reaches the
consensus group, and the denial names neither the roster nor the request body.

## Rejected changes

A promotion of a datacenter that was never added as a learner, or a duplicate node, cluster, or control-endpoint
identity, is refused at once rather than blocking on a quorum it can never reach. The command returns a not-committed
error naming the cause, and the roster is unchanged.
