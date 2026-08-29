+++
title = "Back up and restore"
description = "Capture a verifiable backup of a filesystem-backed data directory and restore it into a fresh node."
weight = 11
+++

A filesystem-backed backup is a portable directory that reproduces a peryx node's durable state: its effective
configuration, its metadata store, and every blob that metadata references. Secrets backed by files or environment
variables remain external dependencies and must be available to the restored node. The backup is offline by
construction. `peryx backup create` reads a stopped or quiescent data directory and writes files with hash checks;
nothing about it talks to a running server or an upstream. That makes a backup portable. Copy it to another host, verify
it there, and restore it into an empty data directory to rebuild the node.

Reach for this when you move a node between hosts, seed a staging environment from production state, or hold a
point-in-time image before a risky migration. It is not continuous replication: for a hot standby that follows the
writer, run a [read replica](@/core/availability/high-availability.md). A backup is the cold copy you keep, verify, and
restore on your own schedule.

## What a backup contains

`backup create` writes a directory with four parts:

- `manifest.json`: the format version, a creation timestamp, the SHA-256 and byte size of every other file, and the
  [availability recovery point](#availability-state) the copy belongs to. It is the root of trust the verifier checks
  everything against.
- `config.toml`: an effective configuration snapshot, the merged result of the config file and any runtime overrides
  rather than a copy of the source file. Restoring it reproduces the settings the node ran with.
- `metadata/peryx.redb`: a copy of the metadata store.
- `blobs.tsv` and `blobs/sha256/…`: a tab-separated index of `sha256`, `size_bytes`, and path, plus one file per blob
  laid out by digest.

The backup copies only referenced blobs. It scans the metadata store for digests named by active owners and copies
exactly those, so an unreferenced blob left behind by an interrupted write does not enter the backup. A backup is
therefore a compaction that carries the live working set, not the on-disk residue. `backup create` rehashes each blob as
it copies it; a blob whose bytes no longer hash to its digest aborts the backup rather than sealing corruption into it.

The configuration snapshot can contain secret values. A secret configured inline remains inline so the effective
configuration can round-trip. This applies to inline signing keys, LDAP bind passwords, OIDC client secrets, replication
tokens, index access tokens, upstream passwords and tokens, and webhook secrets. A secret configured through a `_file`
or `_env` option remains a path or environment variable name; its resolved value is not copied. Provision those
referenced secrets on the restore host before starting the node.

Treat the backup directory and every copy as credential-bearing material. Restrict access to operators authorized to use
the contained credentials, and preserve the same protections when copying or archiving it. If a backup reaches an
unauthorized recipient, rotate every inline credential it contains and create a new backup. Deleting the disclosed copy
does not invalidate credentials that someone may already have read.

On Unix, `backup create` enforces that itself. It creates the backup root `0700` and the secret-bearing files it writes
there, the configuration snapshot, the metadata store, and the manifest, `0600`, regardless of the process umask. A
pre-created target must belong to the effective user. The command opens that directory without following a symlink,
tightens the open directory to `0700`, and creates each member relative to the same descriptor. It confirms the target
still names that directory before writing the manifest. Exclusive, no-follow opens make a member symlink or replaced
directory fail before manifest publication. `restore` applies the same `0600` to the `config.toml` and `peryx.redb` it
lays down. Other platforms carry no Unix mode bits, so protect the directory with the filesystem's own access controls.

## Availability state

A backup is one coherent recovery point, and the manifest names which one. `metadata_frontier` records the metadata
store's control-plane serial at the instant the copy was taken: the store advances it on every committed write, so the
number is the recovery point's identity. `placements` records how many artifacts the store projects a local or remote
availability for, sizing the availability state the metadata carries. For `dc` and `ha`, `writer_identity` records the
writer claim associated with the recovery point. `mode` records whether the node ran in `none`, `dc`, or `ha`, and when
a static datacenter roster is configured, `membership` records its group and every member's node, datacenter, address,
and role. The configuration snapshot omits that roster, so the manifest is the backup's only durable record of the
topology the recovery point belongs to.

`backup create` reads the frontier and placement count from the quiesced metadata store whose bytes it copies, so both
describe exactly the bytes the backup holds rather than a moving target. That is what makes the recovery point coherent:
the metadata copy, the frontier that names it, and the placement count that sizes it all come from the same snapshot.

`backup verify` re-derives the frontier, placement count, and writer identity from the copied store and rejects a
manifest that disagrees, which catches a metadata file swapped for one taken at a different point or edited after the
fact. It also rejects a roster the runtime could never consume: an empty group or member set, a duplicated node,
datacenter, or address, or a member count of writers other than one. A `dc` or `ha` backup and a `none` backup verify
the same way; the availability block is present in every format-2 manifest, empty of a roster when none was configured.

The recovery point objective is the frontier: a restore rebuilds the node as of that serial and loses any write that
landed after the copy, so size the backup interval to how much recent state you can afford to replay from upstream or
recreate from its source. Because the copy is offline, take it against a quiescent writer or a `read_only` node as
[above](#create-a-backup), so the frontier the manifest records matches a metadata file that is not still moving under
it.

## Create a backup

`backup create` takes the same `--config` and `--data-dir` flags as `serve`, followed by the target directory:

```console
$ peryx backup create --data-dir /var/lib/peryx /backups/peryx-2026-08-01
created	/backups/peryx-2026-08-01
metadata	/var/lib/peryx/peryx.redb
blobs	1284	5311746048
```

The target must be empty or absent; the command refuses to write into a directory that already holds files, so a stray
path never merges two backups.

Take the copy against a quiescent writer. `backup create` copies the metadata file directly rather than opening the live
database, so an image captured while the writer commits can be inconsistent. Stop the node first, or run the backup
against one you have switched to `read_only`. A [read replica](@/core/availability/high-availability.md) rejects
mutations and runs no background maintenance, so its metadata holds still while you copy it. Blobs need no such care:
they are content-addressed and immutable once written, so copying them alongside a live reader is safe.

## Verify a backup

`backup verify` rereads a backup and reports whether it can still be trusted, without touching any data directory:

```console
$ peryx backup verify /backups/peryx-2026-08-01
ok
```

Verification rehashes the config snapshot and the blob index against the manifest, rehashes every blob against its own
digest, and confirms the recorded blob count and total bytes match what the index lists. It then opens the copied
metadata store and checks that every digest the metadata references is present in the backup, the property that makes a
restore complete rather than dangling, and confirms the [availability recovery point](#availability-state) the manifest
records still matches the store. Any mismatch prints a `problem` line naming the file and the discrepancy, and the
command exits non-zero:

```console
$ peryx backup verify /backups/peryx-2026-08-01
problem	blob	sha256:1f3a…	sha256 expected sha256:1f3a…, found sha256:9c02…
problems	1
```

Verify on the machine that holds the backup and on the machine that wrote it. A backup that passed at creation but fails
after a copy across hosts or a spell on cold storage has caught bit rot or a truncated transfer before a restore depends
on it. Cheap and read-only, `backup verify` belongs on a schedule against every backup you intend to keep.

## Restore a backup

`restore` writes a backup into a data directory that a server can then serve:

```console
$ peryx restore /backups/peryx-2026-08-01 --data-dir /var/lib/peryx
restored	/var/lib/peryx
blobs	1284	5311746048
bytes	5312060231
elapsed_ms	48213
```

Restore verifies the backup in full before it writes a single byte, so a corrupt backup halts recovery instead of
populating a data directory with damaged state. It refuses a target that already holds files unless you pass `--force`,
which replaces the directory wholesale. That guard keeps a restore from colliding with a node that is still live.
Restore into an empty path, then start the server against it:

```console
$ peryx restore /backups/peryx-2026-08-01 --data-dir /var/lib/peryx --force
$ peryx serve --data-dir /var/lib/peryx
```

The `bytes` and `elapsed_ms` lines report how much the restore read and how long it took, so an operator can size the
recovery time objective from a real run rather than an estimate.

## Cluster identity and rollback

Before `--force` replaces a readable metadata store, restore compares its `writer_identity` with the backup's claim.
Different non-empty labels fail the restore whether the operator passes `--force`:

```console
$ peryx restore /backups/node-b --data-dir /var/lib/peryx --force
Error: refusing to restore node node-b onto a directory claimed by node node-a; clear the target or restore node-a's own backup
```

The guard has no installation identity. It compares the writer labels that both metadata stores record:

| Backup label | Target store                         | Identity result                          |
| ------------ | ------------------------------------ | ---------------------------------------- |
| `node-a`     | readable, label `node-b`             | refuse                                   |
| `node-a`     | readable, label `node-a`             | allow                                    |
| absent       | readable, with or without a label    | allow; no backup label exists to compare |
| any          | readable, label absent               | allow; no target label exists to compare |
| any          | cleared; metadata store absent       | allow; no target store exists to inspect |
| any          | unreadable or held open by a process | fail before replacing any target files   |

The normal empty-target rule follows this identity check, so an occupied target needs `--force`. Clearing a target
removes its writer label and the comparison. `none` mode records no label, and operators can reuse labels across
separate installations; inspect the backup and target before replacing either case.

When both readable stores carry the same label, a forced restore can roll control state backward. If the backup's
recovery point precedes the target's control-plane serial, restore warns about the discarded writes:

```console
warning	restore	rollback	target at serial 4192, backup at 4188
```

For a production-to-staging restore, start with an empty or cleared staging target so no target label blocks the copy.
The restored metadata retains the backup's writer label. Review its
[configuration and credentials](#what-a-backup-contains) and isolate staging from production networks before starting
it; the restore guard does not prove that the two installations have distinct identities.

## Recovery paths

For a `none` deployment, restore rebuilds the whole process: restore the backup into an empty data directory and start
`serve` against it. The restored node resumes at the backup's frontier.

For a datacenter group, restore the **writer's** backup, since the writer's metadata store is the authority the replicas
follow. Start the restored writer, then bring up each replica pointed at it; a replica needs no backup of its own,
because it re-synchronizes its metadata and draws the referenced blobs from the writer once it connects. Restoring a
replica's backup in place of the writer's would seat an out-of-date authority, so keep the writer's backup as the
group's recovery point and let the replicas rebuild from it.

The restored `config.toml` records the `data_dir` the backup came from. When that differs from the `--data-dir` you
restore into, the command still restores but prints a warning, because the snapshot's path no longer matches where the
data now lives:

```console
warning	config	data_dir	backup=/var/lib/peryx	restore=/srv/peryx/data
```

Reconcile it before serving: point the running node at the new location with `--data-dir`, or edit the `data_dir` in the
restored configuration so the snapshot and the on-disk layout agree.

## Object storage backends

`backup create` supports only the local filesystem blob store under `<data_dir>/blobs`. Peryx rejects an
[S3-backed configuration](@/core/operations/configuration.md#blob) before creating the target:

```text
creating an offline backup is only supported on the filesystem blob backend, but this repository is configured for S3; run it against a filesystem-backed repository
```

Peryx writes no manifest, configuration snapshot, or metadata copy after this error. Run `backup create` only on a node
that uses the filesystem backend.

Protect an S3 bucket with the object store's versioning, replication, or backup tooling. That protects blob bytes, but
it does not capture the local Peryx metadata store. Peryx creates no metadata recovery point for an S3-backed node, so
an S3 recovery plan needs an independent metadata backup.

## Operational notes

A backup is a point-in-time image, not a journal: it captures the state at the instant you copy it and knows nothing of
writes that land afterward. Size the interval to how much recent state you can afford to rebuild from upstream, and keep
`backup verify` on a timer so a backup's health is known before the day you need it. Because a backup carries only the
referenced working set, its size tracks live artifacts rather than historical churn. A node that has reclaimed
unreferenced blobs backs up no smaller, since orphans were never in the working set.

For the exact flags on each command, see the [command line reference](@/core/operations/cli.md).
