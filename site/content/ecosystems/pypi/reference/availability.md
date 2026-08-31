+++
title = "Availability behavior"
description = "Python package authority keys, replica reads, upload retries, reclamation references, and log fields."
weight = 8
+++

The PyPI implementation supplies authority keys, replicated-view updates, and blob references to the
[availability contracts](@/core/availability/contracts.md). The
[release-status inventory](@/core/availability/_index.md#release-status) separates its running paths from HA design
material. Startup configuration selects the coordinator:

```toml
[availability]
mode = "none"
```

Omitting the table also selects `none`. This mode commits uploads locally and creates no availability listener, worker,
watcher, or timer; the upload still uses its local ingress ledger. Mode `dc` starts replication without ownership
consensus. Mode `ha` assembles replication and ownership components, which peers reach at the one address the roster
names for each member.

The mode does not change the Simple API or upload form. It changes the durability acknowledgement and failure behavior.
In `dc` and `ha`:

- A read-only replica refuses uploads and mutations with `503 Service Unavailable` before reading the body.
- Repeating the same upload bytes converges on one digest result.
- Hosted and cached project pages remain behind the readable frontier until required views catch up.
- Ingress saturation returns `503 Service Unavailable` with `Retry-After`.

HA code adds a project-home epoch. A publication under a superseded epoch returns `409 Conflict`, while a request that
reaches a node outside the assigned home returns `503 Service Unavailable` with `Retry-After`. `dc` has no project-home
epoch.

## Authority keys

A PyPI authority is a normalized project name. Peryx applies
[PEP 503 name normalization](https://packaging.python.org/en/latest/specifications/name-normalization/): ASCII case
folds, and each run of `.`, `_`, or `-` becomes one `-`. `Flask`, `FLASK`, and `flask` use the authority key `flask`.

PyPI keys have no scheme prefix. A normalized project name cannot contain `:`, which leaves prefixed keyspaces for other
ecosystems.

## Admission and finalization

The upload endpoint records an ingress intent in every mode, then publishes bytes and metadata on the serving node. HA
code assigns the first serving datacenter as the project home and refuses later writes sent elsewhere. No transport
moves an intent from another ingress datacenter to that home.

See [ingress admission and publication](@/ecosystems/pypi/reference/uploads.md#ingress-admission-and-publication) for
request behavior. [Finalizing admitted content](@/core/availability/finalization.md) marks the cross-datacenter intent
transport as design material.

## Derived views

The search index is a required distributed view. Applying a replicated page rebuilds search documents for the projects
named by its changed project markers, upload records, and overrides. The PyPI owner rebuilds one `(index, project)` at a
time from stored records, then advances the search frontier after all affected projects succeed.

A failed project rebuild holds the prior search frontier. A later full refresh rebuilds the index and advances the
frontier after the input becomes readable. Repeating a project rebuild deletes and recreates that project document,
which makes replay idempotent.

## Remote content and reclamation {#reclamation-references}

Under `dc`, a project file or metadata sibling that misses local storage can use
[remote read-through](@/core/repositories/remote-read-through.md) when another configured member has a verified
placement. HA can select another datacenter. Mode `none` has no remote placements.

The PyPI reclamation inventory retains digests named by cached file URL records, PEP 658 metadata siblings, hosted
upload records, and PEP 740 provenance records. Trash and verified placements add the core references described in
[Blob reclamation](@/core/availability/blob-reclamation.md).

The reference inventory ships, but distributed reclamation requires an ownership term, so it performs no work in `dc`.

## Logging

Python repository security actions include `token_use`, `upload`, `yank`, `unyank`, `delete`, `restore`, `promote`, and
`mirror_sync`. PyPI fields include `publisher_id`, `token_id`, `source_index`, `hosted_index`, `project`, `version`,
`filename`, `digest`, `count`, `changed`, and `reason` when the action supplies them.

Availability traces map Python package publication to `publish`, removal to `withdraw` or `delete`, upstream population
to `cache-fill`, and visibility changes to `visibility`.

See [Client behavior across availability modes](@/core/availability/client-behavior.md) and
[Logging](@/core/operations/logging.md) for fields emitted by every ecosystem.
