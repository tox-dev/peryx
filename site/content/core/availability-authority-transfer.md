+++
title = "Authority transfer and drain"
description = "How a confirmed home failure moves an authority to a survivor under the control quorum, and how the writes the old home retained drain into the new one."
weight = 8
+++

An authority, a repository's write ownership, has one home datacenter. The first publish assigns that home to its first
winner. After a permanent home failure, the control quorum moves its authority and retained writes to a survivor. This
page covers the failure threshold, target selection, committed move, and retained-write drain. It is the `ha`
counterpart to the single-writer [failover and recovery](@/core/availability-failover-recovery.md) runbook, and it
builds on [node liveness](@/core/availability-liveness.md) for the failure signal and the
[availability contracts](@/core/availability-contracts.md) for the durability each step preserves.

## Suspicion never moves a home

Liveness aging is a routing hint, not a decision. A home that misses heartbeats becomes
[`Suspect`](@/core/availability-liveness.md) and then, past the dead-after threshold, `Dead`, but neither state moves
its authority. A suspicion indicates delay, and a home that recovers within tolerance keeps everything it owned. Only a
home the tracker has confirmed `Dead` triggers failover. The control quorum then commits the proposal through consensus
before touching any write. Moving a home on a false positive can split ownership, so the quorum waits for a confirmed
failure and a committed decision.

The dead-after threshold and one control-quorum consensus round dominate the failover recovery time. Target selection
uses a bounded in-memory pass. Tune the failover RTO through the liveness thresholds while retaining confirmation.

## Choosing the target

Given a confirmed-dead home, the failover policy picks the datacenter to move it to from the candidates the roster
offers, each carrying the liveness the tracker holds for it. The choice is a single bounded pass:

- Only an `Alive` candidate is eligible. A suspect, dead, or unknown candidate cannot receive a home.
- The first eligible candidate in the caller's order wins. The caller orders the candidates, so the outcome is
  deterministic rather than dependent on map iteration.
- The pass weighs at most a bounded number of candidates, so a long roster cannot stall one decision.

Without a live candidate, the authority stays put and the old home's writes remain retained until a candidate recovers.

## Committing the move

The chosen move commits on the control quorum, which mints the authority's next epoch. That new epoch is the fence: any
write the old home had in flight under the previous epoch is stale, and the new epoch rejects it. A former home that
returns cannot finalize a write against an authority it no longer owns. A datacenter in a control-plane minority cannot
commit the move. It forwards to the leader, so a partition cannot produce two homes.

{% mermaid() %}
flowchart TB
alive["home Alive or Suspect"] -->|"within tolerance"| hold["hold: authority stays"]
dead["home confirmed Dead"] --> pick{"an Alive candidate?"}
pick -->|"no"| none["hold: writes stay retained"]
pick -->|"yes"| commit["control quorum commits the move, mints the fencing epoch"]
commit --> drain["drain the old home's retained intents at the new home"]
class hold,none warn
class commit,drain good
{% end %}

## Draining the retained writes

Moving the home settles ownership; it does not settle the writes the old home was still holding. Before a home finalizes
a write, the ingress datacenter that received it retains it as an intent (see the ingress staging model in the
[availability contracts](@/core/availability-contracts.md)). When the home moves, those intents have to be finalized at
the new home. That is the drain, run with [`peryx job drain`](@/core/cli.md#job-drain):

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

A transfer mints a higher epoch, and the [authority fence](@/core/availability-contracts.md) stops the old home from
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

The system retains a reconciled record until the required-replica and operator audit-retention frontiers pass its
serial. Required replicas can then apply the outcome after a restore, and operators can query it during the audit
window. Releasing records after both frontiers cover them bounds the backlog.

The durable backlog makes the drain restart-safe. A home that stops during reconciliation resumes from pending
operations, and rescanning a settled operation does not reset it. The drain and prune use bounded batches. Alert on
pending backlog depth and drain throughput; a backlog that stops draining means the new home is not settling the old
home's operations.

## Operator recovery

For a confirmed permanent home loss in an `ha` deployment:

1. Confirm that the home is gone rather than suspect. The transfer proceeds only for a `Dead` home, preventing promotion
   on a false positive.
1. Let the control quorum commit the transfer to the selected survivor; a minority cannot, so ensure the quorum is
   reachable.
1. Run `peryx job drain --authority <name>` at the new home to finalize the retained intents. Read the run back with
   `peryx job show` to confirm it succeeded; a run that reports `authority_fenced` raced a further transfer, so re-run
   it at the current home.

Data at risk: nothing acknowledged. The ingress datacenter stores each retained intent, so it survives the home loss and
the drain finalizes it. The caller must retry an unacknowledged in-flight write that did not become an intent.

## Related

- The deliberate operator-initiated move of a healthy home:
  [planned authority transfer](@/core/availability-planned-transfer.md)
- The failure signal that a home is dead: [node liveness](@/core/availability-liveness.md)
- The durability each step preserves: [availability contracts](@/core/availability-contracts.md)
- The single-writer recovery runbook this parallels: [failover and recovery](@/core/availability-failover-recovery.md)
- The `job drain` command and its flags: [command line reference](@/core/cli.md#job-drain)
