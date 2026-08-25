+++
title = "Replication journal"
description = "Which authoritative OCI mutations enter the replication journal and which derived cache state stays local."
weight = 6
+++

When configuration selects `availability.mode = "dc"` or `"ha"`, the OCI implementation records each authoritative
metadata mutation through the replication-journal capability. The journal entry and the rows it describes commit in one
redb write transaction. A replica cannot observe a manifest, tag, or membership change without its matching entry. For
the design pattern, see
[the transactional outbox pattern](https://microservices.io/patterns/data/transactional-outbox.html); for the mutations
themselves, see the [distribution specification](https://github.com/opencontainers/distribution-spec/blob/main/spec.md).

## Journal contents

A hosted push or delete records one typed operation:

- **publish-manifest** records a manifest stored under a repository and includes the tag when the push named one.
  Manifests are content-addressed and immutable, so publishing a manifest and repointing a tag are distinct operations:
  retargeting a tag changes no bytes but is a mutation a replica applies in order.
- **mount-blob** records a blob admitted to a repository's membership, whether pushed directly or mounted from another
  repository.
- **trash-tag** and **trash-manifest** record a soft delete moving a tag, or a digest and each tag that pointed at it,
  into repository trash. A `trash-manifest` entry names the captured tags so a replica trashes the same set.
- **restore-tag** and **restore-manifest** record a restore and name the restored tags. A replica restores those whose
  live slot was free.

## Journal exclusions

A replica reconstructs proxy cache state by pulling upstream. The journal omits cache fills, tag freshness, and cache
evictions after an upstream `404`. A replica also rebuilds referrer descriptors from the pushed manifest bytes in the
`publish-manifest` entry.

## `none` mode

`availability.mode = "none"` installs no distributed availability resources or journal writer. OCI mutations commit
locally and create no journal entry. Configuration selects the mode for the process lifetime.
