+++
title = "Failover and recovery"
description = "Classify availability failures, recover the selected mode, and verify service before restoring traffic."
weight = 10
aliases = [ "/core/availability-failover-recovery/"]
+++

Classify a node failure before changing durable state. Process loss, storage loss, network partition, and control-quorum
loss require different recovery procedures. The selected [availability contract](@/core/availability/contracts.md)
determines the recovery bound and whether recovery is local, same-datacenter, or cross-datacenter.

This runbook applies to shipped `none` and `dc` recovery. HA components are present, but no runtime worker selects a
failed home, so cross-datacenter recovery is an administrator-driven procedure.

Use [high availability](@/core/availability/high-availability.md) for the writer-and-replica model,
[back up and restore](@/core/operations/backup-restore.md) for offline images, and the
[command line reference](@/core/operations/cli.md) for command options.

## Before an incident

Confirm these conditions before an incident:

- For a filesystem-backed node, a verified backup exists and is recent enough. `peryx backup create` writes an offline
  image; `peryx backup verify` reproves it. Keep [verify](@/core/operations/backup-restore.md#verify-a-backup) on a
  timer against every backup you intend to keep, on the host that holds it, so a copy that rotted on cold storage is
  caught before a restore depends on it. Your worst-case data loss is everything committed after the last backup you can
  still verify. For an S3-backed node, maintain separate recovery points for the bucket and the local metadata store
  because `backup create` rejects that configuration.
- For `dc` and `ha`, record the configured `writer_identity`. Promotion and restore check it against the store claim.
- The probes are reachable. `GET /+health`, `GET /+ready`, and, on a `dc` or `ha` node, `GET /+replication/v1/ready` are
  how you tell a recovered node from a lying one. Their fields and the access levels that may read each are on the
  [availability health and readiness](@/core/availability/high-availability.md#availability-health-and-readiness)
  reference.
- You know where blobs live. A local-filesystem store and an
  [S3-compatible bucket](@/core/operations/backup-restore.md#object-storage-backends) recover their bytes differently,
  and the storage-loss procedure below branches on it.

## Classify the failure

{{<diagram file="failover-recovery" />}}

The four failure categories are distinct because the contract distinguishes
[crash from storage loss](@/core/availability/contracts.md#crash-versus-storage-loss) and a
[partition from an outage](@/core/availability/contracts.md#why-a-partition-refuses-instead-of-accepting). Treating any
two as one either restores when a restart would have sufficed or restarts into corruption a restore would have caught.

## Process loss, disk intact

A crash, an OOM kill, or a wedged process with durable storage still present is the benign case. Anything an
acknowledgement covered is `fsync`'d on local disk and survives the restart; a crash is not a data-loss event.

Restart the process against the same data directory:

```console
$ peryx serve --config peryx.toml --data-dir /var/lib/peryx
```

For `dc` and `ha`, startup prepares distributed resources before activation. Failed activation cancels and joins any
resource it started.

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
everything committed after the last backup's serial is lost, and recovery restores a fresh data directory.

First protect against a second writer. If the failed node was the writer, keep it stopped and fenced so it cannot accept
another mutation, and do not start the replacement as a writer until the steps below complete.

For a filesystem-backed node, restore the metadata and referenced local blobs from a verified backup:

```console
$ peryx backup verify /backups/peryx-2026-08-01
ok
$ peryx restore /backups/peryx-2026-08-01 --data-dir /var/lib/peryx
restored	/var/lib/peryx
```

`restore` verifies the whole backup before it writes a byte, so a corrupt image halts recovery instead of seeding a data
directory with damaged state. It refuses a target that already holds files unless you pass `--force`, which replaces the
directory wholesale; the guard prevents a restore from colliding with a live node.

The backup carries the referenced blobs, so the restore above rebuilds them under the data directory.

An S3-backed node cannot use this restore path because `backup create` rejects its configuration before writing
metadata. Recovering the bucket restores blob bytes only; it does not recreate the local metadata store that names them.
Use the independent metadata recovery point from the S3 node's recovery plan, then pair it with the recovered bucket.
See [object storage backends](@/core/operations/backup-restore.md#object-storage-backends).

If the restored node is a replacement writer, promote it as in the [failover](#dc-writer-failover) procedure below. If
it is a replica, start it in replica mode and let it resync.

Validate: `peryx backup verify` passed, `GET /+ready` answers `200`, and for a writer `GET /+ready?writes=true` answers
`200`.

Data at risk: everything after the last verified backup's serial, the `none`
[recovery-point objective](@/core/availability/contracts.md#recovery-objectives). Rollback: a restore into an empty path
is reversible by discarding that directory and choosing a different backup; a `--force` restore over existing files is
destructive and has no rollback, so confirm the target before you pass it.

## Network partition

In `dc` or `ha`, healthy nodes may lose contact: a replica cannot poll the writer, or a load balancer sees a replica
fall behind. With `mode = "none"`, peryx manages no peers, replication feed, or replica frontier. Network failures
affect clients and external storage but cannot split a peryx-managed group because none exists.

Inspect the gap rather than guess at it. On a `dc` or `ha` node, `GET /+replication/v1/ready` names why a replica is not
current in `reasons`:

- `frontier_lag`: the replica has not yet reached the writer's latest serial. Compare its `lag` against the writer's
  write rate; a lag that never reaches zero is a stalled poll, not a slow one.
- `sync_error`: the replica's last poll of the writer failed, the direct symptom of the partition.
- `blob_store`: the mounted blob store failed its reachability check.
- `incompatible_schema`: the writer speaks a replication protocol version the replica cannot apply; a later poll cannot
  resolve without upgrading the writer.

The `serial`, `lag`, and peer origin those responses carry are filtered to `operator:read` and `administration:read`, so
treat that inspection as administrator-only; an anonymous caller reads only `mode`, `role`, `ready`, and `reasons`.

Point read pools at readiness so a lagging replica leaves rotation without a restart, as the
[load-balancer probes](@/core/availability/high-availability.md#load-balancer-probes) section shows. Recovery is to heal
the link: once the poll succeeds the replica advances its frontier and readiness clears on its own. Do not promote a
replica during a partition you have not confirmed as a permanent writer loss, because promoting while the old writer
still runs starts two writers against copies that can diverge.

Data at risk: none. Reads are stale but bounded and self-correct; mutations continue on the unaffected writer.

## HA control-quorum loss

Modes `none` and `dc` run no ownership consensus and have no control quorum to lose. HA command handlers return
`503 Service Unavailable` when they cannot reach consensus, but this is component behavior rather than a deployable
recovery path while HA peer routing remains split.

## DC writer failover

In managed `dc`, offline promotion handles permanent writer loss. It changes the metadata store's writer claim; it does
not copy data or stop the old process. Follow
[DC writer promotion](@/core/availability/high-availability.md#dc-writer-promotion):

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
| Control-quorum loss | not applicable to `none` or `dc`              | HA component has no supported recovery procedure         |

DC recovery is bounded by the replica's applied metadata and blob frontiers. HA recovery objectives remain design goals
until HA has a supported network layout.

## Related

- Availability mode behavior and probes: [high availability](@/core/availability/high-availability.md)
- The offline image every restore reads: [back up and restore](@/core/operations/backup-restore.md)
- What each mode's acknowledgement promised and risks: [availability contracts](@/core/availability/contracts.md)
- The counters and status surface to watch during recovery:
  [monitor](@/core/operations/monitor.md#check-operational-status)
- The exact flags on each command: [command line reference](@/core/operations/cli.md)
