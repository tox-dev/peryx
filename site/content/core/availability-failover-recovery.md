+++
title = "Failover and recovery"
description = "Symptom-driven recovery for the single-writer model: classify the failure, run the matching procedure, and validate service before returning traffic."
weight = 10
+++

When a node fails, the first job is to name the failure, because the four failures peryx can suffer recover along
different paths and the wrong path can turn a recoverable outage into data loss. This guide is symptom-driven: start
from what you observe, classify it into one of four failure classes, run the procedure for that class, and validate
service before you route traffic back. The selected [availability contract](@/core/availability-contracts.md) determines
the recovery bound and whether recovery is local, same-datacenter, or cross-datacenter.

The procedures below reuse the commands and probes defined on their reference pages rather than restate them: the
[high availability](@/core/high-availability.md) page for the writer-and-replica model,
[back up and restore](@/core/backup-restore.md) for the offline image, and the
[availability contracts](@/core/availability-contracts.md) for the durability each acknowledgement promised. The
[command line reference](@/core/cli.md) has the exact flags.

## Before an incident

Recovery depends on state you prepare while healthy, so confirm these before you need them:

- A verified backup exists and is recent enough. `peryx backup create` writes an offline image; `peryx backup verify`
  reproves it. Keep [verify](@/core/backup-restore.md#verify-a-backup) on a timer against every backup you intend to
  keep, on the host that holds it, so a copy that rotted on cold storage is caught before a restore depends on it. Your
  worst-case data loss is everything committed after the last backup you can still verify.
- You know each node's `writer_identity`. Promotion and restore both check it, and a recovery that guesses it stalls at
  the safety check.
- The probes are reachable. `GET /+health`, `GET /+ready`, and, on a `dc` or `ha` node, `GET /+replication/v1/ready` are
  how you tell a recovered node from a lying one. Their fields and the classes that may read each are on the
  [availability health and readiness](@/core/high-availability.md#availability-health-and-readiness) reference.
- You know where blobs live. A local-filesystem store and an
  [S3-compatible bucket](@/core/backup-restore.md#object-storage-backends) recover their bytes differently, and the
  storage-loss procedure below branches on it.

## Classify the failure

{% mermaid() %} flowchart TB; sym["node not serving correctly"] --> disk{"is the durable disk intact?"}; disk -->|"yes,
process is down or wedged"| proc["process loss"]; disk -->|"no, disk or bucket state is gone"| store["storage loss"];
sym --> link{"nodes healthy but cannot reach each other?"}; link -->|"replica cannot follow the writer"| part\["network
partition"\]; sym --> quorum{"a dc or ha mutation refuses with 503?"}; quorum -->|"required failure domain unreachable"|
cq["control-quorum loss"]; class proc,part good; class store,cq warn {% end %}

The four classes are distinct because the contract distinguishes
[crash from storage loss](@/core/availability-contracts.md#crash-versus-storage-loss) and a
[partition from an outage](@/core/availability-contracts.md#why-a-partition-refuses-instead-of-accepting). Treating any
two as one either restores when a restart would have sufficed or restarts into corruption a restore would have caught.

## Process loss, disk intact

A crash, an OOM kill, or a wedged process with durable storage still present is the benign case. Anything an
acknowledgement covered is `fsync`'d on local disk and survives the restart; a crash is not a data-loss event.

Restart the process against the same data directory:

```console
$ peryx serve --config peryx.toml --data-dir /var/lib/peryx
```

Validate before returning traffic: `GET /+ready` must answer `200` with `{"status":"ready"}`, and for a writer,
`GET /+ready?writes=true` must also answer `200`. Do not treat `GET /+health` as recovery evidence: it reports only that
the process can answer at all and stays `200` through a metadata or blob-store failure a restart cannot repair.

Data at risk: none acknowledged. The freshness cache is deliberately non-durable, so a crash can drop a cached page and
cost one refetch from upstream, but no acknowledged mutation is lost. Rollback: none needed; the restart is
non-destructive because it opens the same durable state.

If the process will not become ready, the disk is not intact after all. Read the readiness reason and move to storage
loss.

## Storage loss

The disk is gone, or the metadata store is corrupt. In `none` this is the single-failure-domain event the mode names:
everything committed after the last backup's serial is lost, and recovery is a restore into a fresh data directory
followed, if the lost node was the writer, by promotion.

First protect against a second writer. If the failed node was the writer, keep it stopped and fenced so it cannot accept
another mutation, and do not start the replacement as a writer until the steps below complete.

Restore the metadata and referenced local blobs from a verified backup:

```console
$ peryx backup verify /backups/peryx-2026-08-01
ok
$ peryx restore /backups/peryx-2026-08-01 --data-dir /var/lib/peryx
restored	/var/lib/peryx
```

`restore` verifies the whole backup before it writes a byte, so a corrupt image halts recovery instead of seeding a data
directory with damaged state. It refuses a target that already holds files unless you pass `--force`, which replaces the
directory wholesale; that guard is what keeps a restore from colliding with a node that is still live.

Blob bytes recover by backend:

- **Local filesystem.** The backup carries the referenced blobs, so the restore above rebuilds them under the data
  directory. Nothing more is needed.
- **S3-compatible bucket.** The backup carries the metadata and the configuration that address the bucket, not the
  object bytes. Recover the bucket with the object store's own tooling, versioning, replication, or a lifecycle-managed
  copy, and pair the restored metadata with a bucket that already holds the referenced objects. See
  [object storage backends](@/core/backup-restore.md#object-storage-backends).

If the restored node is a replacement writer, promote it as in the [failover](#writer-failover) procedure below. If it
is a replica, start it in replica mode and let it resync.

Validate: `peryx backup verify` passed, `GET /+ready` answers `200`, and for a writer `GET /+ready?writes=true` answers
`200`.

Data at risk: everything after the last verified backup's serial, the `none`
[recovery-point objective](@/core/availability-contracts.md#recovery-objectives). Rollback: a restore into an empty path
is reversible by discarding that directory and choosing a different backup; a `--force` restore over existing files is
destructive and has no rollback, so confirm the target before you pass it.

## Network partition

The nodes are healthy but cannot reach each other: a replica cannot poll the writer, or a load balancer sees a replica
fall behind. In `none` there is exactly one writer identity, claimed in the metadata store at startup, so a partition
cannot produce two writers and cannot corrupt authoritative state. The writer keeps accepting mutations; each replica
keeps serving the state it holds, bounded by its
[frontier](@/core/availability-contracts.md#the-frontier-bounds-staleness), the highest serial it has durably applied.

Inspect the gap rather than guess at it. On a `dc` or `ha` node, `GET /+replication/v1/ready` names why a replica is not
current in `reasons`:

- `frontier_lag` : the replica has not yet reached the writer's latest serial. Compare its `lag` against the writer's
  write rate; a lag that never reaches zero is a stalled poll, not a slow one.
- `sync_error` : the replica's last poll of the writer failed, the direct symptom of the partition.
- `blob_store` : the mounted blob store failed its reachability check.
- `incompatible_schema` : the writer speaks a replication protocol version the replica cannot apply; this one a later
  poll cannot resolve without upgrading the writer.

The `serial`, `lag`, and peer origin those responses carry are filtered to `operator:read` and `administration:read`, so
treat that inspection as administrator-only; an anonymous caller reads only `mode`, `role`, `ready`, and `reasons`.

Point read pools at readiness so a lagging replica leaves rotation without a restart, as the
[load-balancer probes](@/core/high-availability.md#load-balancer-probes) section shows. Recovery is to heal the link:
once the poll succeeds the replica advances its frontier and readiness clears on its own. Do not promote a replica
during a partition you have not confirmed as a permanent writer loss, because promoting while the old writer still runs
starts two writers against copies that can diverge.

Data at risk: none. Reads are stale but bounded and self-correct; mutations continue on the unaffected writer.

## Control-quorum loss

`none` runs no consensus and has no quorum to lose, so this class does not arise on what peryx ships today: a writer
that loses its replicas keeps writing, and the recovery for losing the writer itself is the storage-loss restore and
failover above.

It is named here because the `dc` and `ha` modes the
[contract](@/core/availability-contracts.md#why-a-partition-refuses-instead-of-accepting) defines do have a quorum, and
their behavior under its loss is already fixed: an authoritative mutation that cannot reach the failure domain its
acknowledgement names refuses with `503 Service Unavailable` rather than commit locally and return a success that lies.
Reads do not refuse; a partitioned node keeps serving its frontier. When those modes ship, a quorum-loss runbook belongs
here; until then, a `503` from a mutation is the contract refusing, and the recovery is to restore the failure domain,
not to force the write.

## Writer failover

Promotion is the recovery for a permanent writer loss. It changes the metadata store's writer claim; it does not copy
data, elect a leader, or stop the old process, so the ordering below is the safety, not a formality. The reference
procedure is [manual promotion](@/core/high-availability.md#manual-promotion); as a recovery it runs:

1. **Fence the old writer.** Stop it so it cannot accept another mutation. This is the rollback boundary: until you
   promote, you can still abandon the failover and bring the original writer back.

1. **Converge and verify the replacement.** Finish copying the writer's metadata and blobs to the selected replica and
   verify the copy, so the promoted node carries the furthest-forward state you have.

1. **Promote.** With the replica stopped and still configured with the old identity, replace the store's claim:

   ```console
   $ peryx writer promote writer-b --config peryx.toml
   ```

   The command compares the configured identity with the store's current claim and refuses a stale or missing value, so
   a copy that diverged cannot silently take over.

1. **Reconfigure and start.** Set `writer_identity = "writer-b"`, remove replica mode, and start the node.

1. **Validate.** Wait for `GET /+ready?writes=true` to answer `200`, then move write traffic to it.

1. **Rebuild.** Bring former writer nodes back only as replicas. Past the promotion the original writer is no longer a
   rollback target; two writers against copies that can diverge is the one outcome the procedure exists to prevent.

Data at risk: whatever the promoted replica had not yet copied from the failed writer, bounded by the replica's frontier
at promotion. Converging the copy in step 2 is what shrinks that window.

## Validate a recovery

Whatever the class, a node is recovered only when it proves it:

- `peryx backup verify` passed, if the path involved a restore.
- `GET /+ready` answers `200` with `{"status":"ready"}`, and a writer also answers `200` on `GET /+ready?writes=true`.
- On a `dc` or `ha` node, `GET /+replication/v1/ready` answers `200` with an empty `reasons`, confirming the frontier is
  current.
- `GET /+status` shows the expected topology to an `administration:read` caller, the administrator-only confirmation
  that the node took the role you intended.

Only then return traffic. A node answering `/+health` but failing `/+ready` is live but not serving, and routing to it
turns a contained outage into a visible one.

## Recovery objectives by failure class

| Failure class       | Data at risk (`none`)                         | Return to service                                        |
| ------------------- | --------------------------------------------- | -------------------------------------------------------- |
| Process loss        | none acknowledged                             | restart against the same data directory                  |
| Storage loss        | everything after the last verified backup     | restore into a fresh directory, then promote if a writer |
| Network partition   | none; reads stale but bounded by the frontier | heal the link; the replica advances its frontier         |
| Control-quorum loss | not applicable to `none`                      | `dc` and `ha` refuse the mutation; restore the domain    |

The `dc` and `ha` columns of these bounds are the stronger
[recovery objectives](@/core/availability-contracts.md#recovery-objectives) the contract states as a serial, not a
duration: "no acknowledged mutation at or before frontier *n*", so a mode is measured by which serials it can recover,
not by a stopwatch.

## Related

- The single-writer model these procedures operate: [high availability](@/core/high-availability.md)
- The offline image every restore reads: [back up and restore](@/core/backup-restore.md)
- What each mode's acknowledgement promised and risks: [availability contracts](@/core/availability-contracts.md)
- The counters and status surface to watch during recovery: [monitor](@/core/monitor.md#check-operational-status)
- The exact flags on each command: [command line reference](@/core/cli.md)
