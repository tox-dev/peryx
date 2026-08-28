+++
title = "Store blobs in object storage"
description = "Move content-addressed blobs to an S3-compatible bucket so several nodes share one durable store."
weight = 11
+++

By default peryx writes blobs to the local filesystem under `data_dir/blobs`, which suits a one-node deployment where
one process owns one disk. Point the `[blob]` table at an S3-compatible bucket when the filesystem stops fitting the
deployment. Metadata never moves; the redb store stays local on every node. Only the content-addressed blob bytes
relocate to the bucket.

Reach for object storage when one of these holds:

- Several nodes must serve the same artifacts. A [high-availability](@/core/availability/high-availability.md)
  deployment runs one writer and read replicas, and peryx copies neither blobs nor metadata between them. Because blob
  keys are the content digest, every node configured against the same bucket reads the same immutable objects, so you
  replicate only the metadata store and leave the blob bytes in one shared place.
- You want the object store to own durability. Bucket-level versioning, cross-region replication, and lifecycle rules
  are the object store's job, not peryx's.
- The cache outgrows one volume. A bucket scales past the disk a single host can attach.

If none of these apply, keep the filesystem default. It commits within one host's failure domain behind an atomic rename
and needs no external service.

## Point a node at MinIO

MinIO and other self-hosted gateways serve path-style URLs (`https://host/bucket/key`), so set `path_style = true`.
Create the bucket first, then configure the backend:

```toml
[blob]
backend = "s3"
endpoint = "https://minio.internal:9000"
bucket = "peryx-blobs"
region = "us-east-1"
prefix = "prod"
path_style = true
```

peryx resolves credentials through the AWS SDK default provider chain, so the `[blob]` table holds no secret. For a
static MinIO key pair, export the standard environment variables before starting the process:

```bash
export AWS_ACCESS_KEY_ID=peryx
export AWS_SECRET_ACCESS_KEY=...
```

A completed write proves two guarantees, `conditional_writes` and `checksum_writes`, both `true` by default. AWS S3
honors an `If-None-Match: *` create-if-absent write and validates the SHA-256 checksum sent with each object; some
S3-compatible gateways reject the `*` precondition or the checksum header. Setting a guarantee to `false` stops peryx
from sending its header, so a node keeps writing against an endpoint that rejects it. If yours does, set the missing one
to `false`:

```toml
conditional_writes = false
checksum_writes = false
```

That declaration is per instance and restricts the node to the `none`
[availability contract](@/core/availability/contracts.md): the `dc` and `ha` modes require both guarantees, and startup
refuses one of those modes on a backend that declares either as `false`.

## Point a node at AWS S3

AWS buckets use virtual-hosted addressing (`https://bucket.s3.region.amazonaws.com`), which is the `path_style = false`
default. Give the regional endpoint and a matching signing region:

```toml
[blob]
backend = "s3"
endpoint = "https://s3.us-east-1.amazonaws.com"
bucket = "peryx-blobs"
region = "us-east-1"
prefix = "prod"
```

On EC2 or ECS, leave the credential variables unset and attach a role instead: the provider chain reads ECS task
credentials or EC2 instance metadata and refreshes them on its own. The instance's role needs a policy that covers the
key prefix plus the bucket-level health check:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:AbortMultipartUpload"
      ],
      "Resource": "arn:aws:s3:::peryx-blobs/prod/*"
    },
    {
      "Effect": "Allow",
      "Action": "s3:GetBucketLocation",
      "Resource": "arn:aws:s3:::peryx-blobs"
    }
  ]
}
```

The object actions land on `<prefix>/*` because peryx keys every blob `<prefix>/sha256/<digest>`. `s3:GetBucketLocation`
sits on the bucket itself because the readiness probe uses it, and `s3:AbortMultipartUpload` lets an interrupted
multipart upload clean up its parts.

## Verify the backend

The readiness probe checks the blob store on every call, so it confirms in one request that credentials and permissions
resolved. After starting the node:

```bash
curl -fsS http://127.0.0.1:4433/+ready
```

A healthy backend answers `200 OK` with `{"status":"ready"}`. A denied `s3:GetBucketLocation`, a wrong region, or an
unreachable endpoint fails the probe with `503 Service Unavailable` and `{"status":"not_ready"}` even when object reads
and writes would otherwise succeed, so treat a not-ready node as a permissions or connectivity problem before anything
else.

`GET /+status` names the resolved backend and its health so you can confirm the node opened the bucket you meant:

```bash
curl -fsS http://127.0.0.1:4433/+status | jq '{backend: .blob_storage.backend, health: .health.blob_store}'
```

It reports `"backend": "s3"` and `"health": "healthy"`. The
[monitoring guide](@/core/operations/monitor.md#check-operational-status) covers the rest of that document.

## Operate an S3 backend

A few behaviors differ from the filesystem default and catch operators out:

- Offline maintenance commands work on the local filesystem store, not the bucket. `peryx fsck` and the `peryx cache`
  blob scans report an unsupported-operation error against an S3 backend, and `peryx import-dir` stages into the local
  `data_dir/blobs` tree regardless of the `[blob]` selection. Import hosted artifacts through the running server and
  lean on the object store's own tooling to audit S3 objects.
- Pick the backend before you host content. Changing `backend` does not move existing blobs, and the filesystem layout
  (`sha256/<ab>/<cd>/<digest>`) does not match the flat S3 key (`<prefix>/sha256/<digest>`), so copying the tree by hand
  will not line the keys up. Cached content repopulates from upstream on demand; ingest hosted content again.
- `part_size_bytes` must fall between 5 MiB and 5 GiB, the multipart bounds S3 enforces. Startup rejects a value outside
  that range.
- `peryx backup create` rejects an S3-backed configuration before writing its target, so it captures neither metadata
  nor objects. Configure versioning, replication, or backups on the bucket for blob recovery, and maintain a separate
  recovery point for Peryx's local metadata store. The [backup and restore](@/core/operations/backup-restore.md) command
  applies only to filesystem-backed nodes.

The [`[blob]` reference](@/core/operations/configuration.md#blob) lists every key, its default, and the full durability
model.
