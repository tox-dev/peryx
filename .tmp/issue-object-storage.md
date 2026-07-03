## Summary

Add pluggable blob storage with an S3-compatible backend.

Depends on #31 for mirror use cases. Supports future #23 HA and replication work.

## Problem

Velox stores blobs on the local filesystem under `velodex-data/blobs`. That is simple and fast for one node, but it ties
cache size, backup, restore, and mirror growth to local disk.

Large mirrors and production deployments need object storage for package blobs. Metadata can stay in redb for this
issue; the first boundary should be package-file storage.

## Competitor reference

Bandersnatch supports filesystem and S3 storage, including S3-compatible endpoints, verify concurrency, retry settings,
and object metadata for `sha256` and upload time.

Nexus exposes blob stores, including S3, Azure Blob Store, and Google Cloud Blob Store.

References:

- https://github.com/pypa/bandersnatch/blob/main/docs/storage_options.md
- https://help.sonatype.com/en/create-a-pypi-repository.html

## Proposed scope

- Add a blob storage backend boundary for:
  - `put`
  - `get`
  - `head`
  - `range`
  - `delete`
  - `verify`
- Keep the filesystem backend as the default.
- Preserve the existing digest-keyed layout for filesystem storage.
- Add S3-compatible config:
  - bucket
  - prefix
  - endpoint URL
  - region
  - path-style flag
  - retry limit
  - timeout
- Use AWS default credential resolution instead of storing access keys in Velox config.
- Store blobs by sha256 digest so writes stay immutable.
- Upload blob bytes before committing metadata. If metadata commit fails, leave orphan blobs for a cleanup command.
- Support range reads so metadata extraction and #15 can avoid loading full artifacts.
- Add backend-specific errors that name the digest, operation, backend, and source error.

## Out of scope

- moving metadata out of redb
- multi-writer HA
- static mirror export
- object lifecycle policy management
- browser storage administration
- migrations that rewrite existing blob keys

## Acceptance criteria

- Existing filesystem-backed deployments keep working without config changes.
- Operators can configure an S3-compatible blob backend without putting secrets in config.
- Blob reads, writes, range reads, deletes, and verification work through the backend boundary.
- Upload and mirror paths commit metadata only after blob writes succeed.
- Failed metadata commits leave detectable orphan blobs.
- Tests cover filesystem behavior and S3-compatible behavior, with MinIO or another opt-in integration target.
