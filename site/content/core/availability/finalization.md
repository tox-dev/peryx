+++
title = "Finalizing admitted content"
description = "Distinguish local PyPI and OCI finalization from the cross-datacenter design."
weight = 8
aliases = [ "/core/availability-finalization/"]
+++

PyPI ships local intent admission and publication in the upload request. Every mode stages an intent, claims the
operation, performs the HA first-home check when configured, commits blob and metadata state, advances the intent to
`admitted`, and checks acknowledgement evidence. A request at a non-home HA node returns `503 Service Unavailable`;
there is no transport that sends its intent to the home for later finalization.

OCI blob pushes stage an intent in the opposite order, because a layer is content-addressed. The push commits and
digest-verifies its bytes first, then retains an intent naming the index, repository, canonical authority, digest,
committed length, operation ID, upload session, and quota reservation, and only then records the repository membership
that makes the layer servable. Bytes left by a crash before the intent lands are an unreferenced orphan that content
cleanup reclaims; a membership never exists without a retained record a home can replay. Manifests do not use this
admission path: metadata holds their bytes, so they have no ingress content to retain.

The recovery finalizer below ships for intents and file rows in the same metadata store. The cross-datacenter extension
requires a future intent and byte transport to the [home datacenter](@/core/availability/home-assignment.md).

Content owners define admission requests and client responses. Finalization consumes owner-neutral intent, placement,
authorization, and operation contracts.

## States

The finalizer reads a durable ingress intent and moves it forward:

| State      | Meaning                                                                                           |
| ---------- | ------------------------------------------------------------------------------------------------- |
| `pending`  | The ingress staged the intent; local publication may need a retry or crash-recovery sweep.        |
| `admitted` | Local publication committed; acknowledgement evidence and the operation outcome may still follow. |
| `expired`  | No upload can finalize the intent; it holds capacity only until the reaper prunes it.             |

An intent reaches `expired` only on the sweep's own evidence. Each pass that finds nothing it could finalize counts
against the intent; after three the sweep stops offering it, and the write-ledger reaper expires it once its staging
deadline has also passed. Age alone never expires a pending intent, so a home that is slow to finalize does not lose a
write whose bytes are durable. A PyPI upload that fails before storing anything releases its intent as it returns rather
than waiting for that deadline. An OCI push has nothing to release: it stages its intent only once the content is
durable, so a fenced or faulted publication leaves a record the home finalizes rather than a write the client loses.

A successful publication records an operation outcome under the admission operation ID. A retry reads this record and
returns the stored acknowledgement. A refusal records no terminal outcome.

## Validation

The finalizer runs these checks before publication:

| Check            | Refusal condition                                                      |
| ---------------- | ---------------------------------------------------------------------- |
| Authority fence  | The authority is unassigned or the request carries a superseded epoch. |
| Content identity | The digest or byte size differs from the admission record.             |
| Placement        | No verified placement can supply the content.                          |
| Authorization    | The principal no longer has write access to the repository.            |

The authority fence runs first. A former home cannot publish work after an
[authority transfer](@/core/availability/authority-transfer.md) advances the epoch.

## Shipped OCI commit boundary

A blob push claims its repository home, reserves quota, commits and verifies the bytes, retains the intent, then commits
the membership, the quota reservation, the upload-session close, and the replication-journal entry in one metadata
transaction. It settles the intent and answers `201 Created` only once the acknowledgement policy proves the write
durable. Until that membership commits, the layer is absent from the repository: a pull answers `404 Not Found` even
though the bytes are stored.

A push the authority fence turns away answers `503 Service Unavailable` and keeps its intent, its reservation, and its
pending operation. The OCI recovery sweep reads those intents on each maintenance tick, revalidates the index, its
upload policy, the content, and the current home and epoch, then publishes the membership the request could not. An
[operator drain](@/core/operations/cli.md) reaches the same publish one intent at a time, and the finalizer checks the
authority on the staging record against the one being drained before publishing under it. A push whose admission cannot
retain an intent is shed with `503` and publishes nothing.

The membership commit runs before the intent settles, so a pass interrupted between the two leaves the intent pending
and the next one republishes to the same state. Nothing settles a write that was never published.

## Shipped PyPI commit boundary

The request path commits blob, metadata, and replication-journal state before it waits for acknowledgement evidence,
then advances the intent to `admitted`. A `202 Accepted` response means publication committed but the evidence deadline
elapsed; a retry rechecks the same operation. Recording `published` follows a successful acknowledgement.

The local PyPI recovery sweep reads pending intents whose file rows are in the same store. Its finalizer validates the
fence, identity, placement, and current write grant, then commits the file rows, replication journal, operation outcome,
and intent advance in one transaction. It does not call the distributed acknowledgement resolver, so its placement check
can record `published` without the DC receipt evidence required by the request path. It cannot read rows or bytes from
another datacenter.

This transaction also defines visibility. Pending or refused content remains absent from client views.

## Retry and restart

Finalization uses the operation ID as its idempotency key:

| Prior outcome | Result                                                                  |
| ------------- | ----------------------------------------------------------------------- |
| none          | Validate current state, then publish or refuse.                         |
| `published`   | Return the stored success without another publication or journal entry. |

A transient refusal can succeed after its cause clears because it creates no terminal outcome. A lost response or a
restart can trigger another attempt, but one operation ID can produce one committed publication.

## Design: remote placement

The design allows finalization from a verified placement elsewhere in the topology. The shipped local PyPI path stores
the upload before it commits metadata; it does not hand a retained ingress upload to a remote home.
