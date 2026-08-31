+++
title = "Authority transfer and drain"
description = "Separate automatic-failover design from the shipped HA transfer and drain components."
weight = 8
aliases = [ "/core/availability-authority-transfer/"]
+++

Automatic failover remains design-only: `FailoverPolicy` and liveness selection have no runtime caller. The HA control
listener ships a raw `transfer_authority` command and `peryx job drain` ships, but the command does not require a `Dead`
home, so an operator drives every transfer. Mode `dc` has no ownership consensus; use offline
[writer promotion](@/core/availability/high-availability.md#dc-writer-promotion).

An authority gives one datacenter write ownership for a repository. The design below uses
[node liveness](@/core/availability/liveness.md) for the failure signal and the
[availability contracts](@/core/availability/contracts.md) for durability.

Each ecosystem derives authority keys and handles retained operations. Peryx ships epoch storage and drain primitives;
automatic target selection is not connected to the service runtime.

## Design: suspicion never moves a home

Liveness aging guides routing. A home that misses heartbeats becomes [`Suspect`](@/core/availability/liveness.md), then
`Dead` after the configured threshold. Only `Dead` permits failover. The control quorum must commit the transfer before
any write moves; this prevents a false suspicion from splitting ownership.

The dead-after threshold and one control-quorum consensus round dominate the failover recovery time. Target selection
uses a bounded in-memory pass. Tune the failover RTO through the liveness thresholds while retaining confirmation.

## Design: choosing the target

Given a confirmed-dead home, the failover policy picks the datacenter to move it to from the candidates the roster
offers, each carrying the liveness the tracker holds for it. The choice is a single bounded pass:

- Only an `Alive` candidate is eligible. A suspect, dead, or unknown candidate cannot receive a home.
- The first eligible candidate in the caller's order wins. The caller orders the candidates, so the outcome is
  deterministic rather than dependent on map iteration.
- The pass weighs at most a bounded number of candidates, so a long roster cannot stall one decision.

Without a live candidate, the authority stays put and the old home's writes remain retained until a candidate recovers.

## Shipped HA command component

`POST /availability/v1/commands` accepts `transfer_authority` and commits the requested move on the control quorum. The
handler does not inspect liveness or select the target. The new epoch is the fence: any write the old home had in flight
under the previous epoch is stale, and the new epoch rejects it. A former home that returns cannot finalize a write
against an authority it no longer owns. A datacenter in a control-plane minority cannot commit the move. It forwards to
the leader, so a partition cannot produce two homes.

{{<diagram file="authority-transfer" />}}

## Draining the retained writes

Moving the home settles ownership; it does not settle the writes the old home was still holding. Before a home finalizes
a write, the ingress datacenter that received it retains it as an intent (see the ingress staging model in the
[availability contracts](@/core/availability/contracts.md)). The shipped intent store is local to the ingress node; no
transport moves arbitrary ingress intents to another datacenter. A local drain runs with
[`peryx job drain`](@/core/operations/cli.md#job-drain):

- **Ordered.** It finalizes retained intents in admission order, held by a durable never-reused sequence that survives a
  restart, so the drain is deterministic and two operators running it reach the same result.
- **Resumable.** Each finalize advances an intent without reapplying it. An interrupted drain resumes at the first
  pending intent, and rerunning a completed drain is a no-op.
- **Bounded.** It finalizes in batches, so a large backlog drains in bounded transactions rather than one unbounded
  scan.
- **Fence-protected.** The run leases the authority's committed epoch. If the authority transfers during the drain, the
  fence rejects the run's success under the superseded epoch with `authority_fenced`. Rerun the drain at the current
  home.

Each retained operation reaches one outcome: the new home finalizes it, or a newer write supersedes it. A home loss at
the transfer boundary leaves one home and one settled outcome per operation, without a double-write or dropped write.

## Reconciling old-epoch operations

A transfer mints a higher epoch, and the [authority fence](@/core/availability/contracts.md) stops the old home from
applying more work under the epoch it lost. The operations it durably recorded before the transfer still sit in its log,
though, and each needs one terminal disposition so the two homes never disagree about what the authority did.

Reconciliation classifies each operation from its committed record, epoch, and current metadata into one of four
outcomes. An operation whose effect exists in committed state is **already applied** and becomes a no-op. A newer
operation can overwrite a durable operation, making it **superseded**. An operation that did not reach durable commit
**failed** with nothing to apply. A durable operation that remains current is **replayable**, and the new home reissues
it under the current epoch. Fixed precedence makes the outcome independent of evaluation order. A never-committed
operation fails first, while an applied operation remains a no-op even if a later operation superseded it because
idempotency settled its effect.

A replay reissues the operation under the new epoch while retaining its original source and serial for idempotency. It
continues the original W3C trace, preserving the audit identity across the transfer. A replay that reaches an authority
past that serial is a no-op, so a retried or duplicated reconciliation does not apply twice.

The distributed coordinator retains a reconciled record until the required-replica and operator audit-retention
frontiers pass its serial. Required replicas can then apply the outcome after a restore, and operators can query it
during the audit window. Releasing records after both frontiers cover them bounds the backlog.

The durable backlog makes the drain restart-safe. A home that stops during reconciliation resumes from pending
operations, and rescanning a settled operation does not reset it. The drain and prune use bounded batches. Alert on
pending backlog depth and drain throughput; a backlog that stops draining means the new home is not settling the old
home's operations.

## Manual HA component exercise

No supported automatic recovery procedure ships. In a development environment that supplies the missing HA peer routing,
an administrator can exercise the raw components:

1. Confirm through external operational evidence that the old home is fenced. The server does not enforce the liveness
   check.

1. Choose the target and submit the raw command to the HA leader:

   ```text
   POST /availability/v1/commands
   Idempotency-Key: recover-proj-west
   { "type": "transfer_authority", "authority": "proj", "new_home": "west" }
   ```

1. Run `peryx job drain --authority <name>` at the new home to finalize the retained intents. Read the run back with
   `peryx job show` to confirm it succeeded; a run that reports `authority_fenced` raced a further transfer, so re-run
   it at the current home.

This sequence is not a durability claim. The HA layout is not deployable, and intents stored only on a lost ingress node
cannot be drained from a survivor.

## Related

- The deliberate operator-initiated move of a healthy home:
  [planned authority transfer](@/core/availability/planned-transfer.md)
- The failure signal that a home is dead: [node liveness](@/core/availability/liveness.md)
- The durability each step preserves: [availability contracts](@/core/availability/contracts.md)
- Writer recovery procedures: [failover and recovery](@/core/availability/failover-recovery.md)
- The `job drain` command and its flags: [command line reference](@/core/operations/cli.md#job-drain)
