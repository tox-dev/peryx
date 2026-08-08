+++
title = "Availability behavior"
description = "Python package authority keys, replica reads, upload retries, reclamation references, and log fields."
weight = 8
+++

Python package routes use the shared [availability contracts](@/core/availability-contracts.md).

- A read-only replica refuses uploads and mutations with `503 Service Unavailable` before reading the upload.
- A publication under a superseded authority epoch returns `409 Conflict`. Retry the unchanged request against the
  current writer.
- Repeating the same upload bytes converges on the digest result.
- Hosted and cached project pages remain behind the readable frontier until required views catch up.
- Ingress saturation returns `503 Service Unavailable` with `Retry-After`.

Availability mode does not change the Simple API or upload form. It changes the acknowledgement point.

## Authority keys

A PyPI authority is a normalized project name. Peryx applies
[PEP 503 name normalization](https://packaging.python.org/en/latest/specifications/name-normalization/): ASCII case
folds, and each run of `.`, `_`, or `-` becomes one `-`. `Flask`, `FLASK`, and `flask` use the authority key `flask`.

PyPI keys have no scheme prefix. A normalized project name cannot contain `:`, which leaves prefixed keyspaces for other
ecosystems.

## Admission and finalization

The upload endpoint records a durable ingress intent after streaming and validating bytes. The authority home finalizes
that intent. It rechecks the epoch, digest, size, placement, and write grant before publishing metadata.

See [Durable ingress admission](@/ecosystems/pypi/reference/uploads.md#durable-ingress-admission) for request behavior
and [Finalizing admitted content](@/core/availability-finalization.md) for the shared transaction.

## Derived views

The search index is a required view. Applying a replicated page rebuilds search documents for the projects named by its
changed project markers, upload records, and overrides. The driver rebuilds one `(index, project)` at a time from stored
records. It advances the search frontier after all projects affected by that page have rebuilt.

A failed project rebuild holds the prior search frontier. A later full refresh rebuilds the index and advances the
frontier after the input becomes readable. Repeating a project rebuild deletes and recreates that project document,
which makes replay idempotent.

## Remote content and reclamation {#reclamation-references}

A project file or metadata sibling that misses local storage can use
[remote read-through](@/core/remote-read-through.md) when another datacenter has a verified placement.

The reclamation inventory retains digests named by cached file URL records, PEP 658 metadata siblings, hosted upload
records, and PEP 740 provenance records. Trash and verified placements add the shared references described in
[Blob reclamation](@/core/availability-blob-reclamation.md).

## Logging

Python repository security actions include `token_use`, `upload`, `yank`, `unyank`, `delete`, `restore`, `promote`, and
`mirror_sync`. Driver fields include `publisher_id`, `token_id`, `source_index`, `hosted_index`, `project`, `version`,
`filename`, `digest`, `count`, `changed`, and `reason` when the action supplies them.

Availability traces map Python package publication to `publish`, removal to `withdraw` or `delete`, upstream population
to `cache-fill`, and visibility changes to `visibility`.

See [Client behavior across availability modes](@/core/availability-client-behavior.md) and [Logging](@/core/logging.md)
for shared fields.
