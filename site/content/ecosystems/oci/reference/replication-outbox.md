+++
title = "Replication outbox"
description = "Which authoritative OCI mutations peryx journals for replicas to reconcile, which cache state it never journals, and why single-node none mode records nothing."
weight = 6
+++

A `dc` or `ha` node records each authoritative OCI metadata mutation in the driver-transaction outbox, so a replica
reconciles every change exactly once and in order. The entry and the rows it describes commit in one redb write
transaction: either both land or neither does, so a replica never observes a manifest, tag, or membership without its
entry, and a crash between the two is impossible. For the concept, see
[the transactional outbox pattern](https://microservices.io/patterns/data/transactional-outbox.html); for the mutations
themselves, see the [distribution specification](https://github.com/opencontainers/distribution-spec/blob/main/spec.md).

## What a node journals

A hosted push or delete records one typed operation:

- **publish-manifest** records a manifest stored under a repository and includes the tag when the push named one.
  Manifests are content-addressed and immutable, so publishing a manifest and repointing a tag are distinct operations:
  retargeting a tag changes no bytes but is a mutation a replica applies in order.
- **mount-blob** records a blob admitted to a repository's membership, whether pushed directly or mounted from another
  repository.
- **trash-tag** and **trash-manifest** record a soft delete moving a tag, or a digest and each tag that pointed at it,
  into repository trash. A `trash-manifest` entry names the captured tags so a replica trashes the same set.
- **restore-tag** and **restore-manifest** record a restore and name the tags it relit. A replica restores those whose
  live slot was free.

## What a node never journals

A replica reconstructs proxy cache state by pulling upstream. The outbox omits cache fills, tag freshness, and cache
evictions after an upstream `404`. A replica also rebuilds referrer descriptors from the pushed manifest bytes in the
`publish-manifest` entry.

## none mode

Single-node `none` carries no replica to reconcile, so its authoritative mutations record no outbox entry and its write
count is unchanged from a build without the outbox. The mode is chosen once, from the `[availability]` table, and fixed
for the life of the process.
