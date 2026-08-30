+++
title = "Finalizing admitted content"
description = "Distinguish local PyPI finalization from the cross-datacenter design."
weight = 8
aliases = [ "/core/availability-finalization/"]
+++

PyPI ships local intent admission and publication in the upload request. Every mode stages an intent, claims the
operation, performs the HA first-home check when configured, commits blob and metadata state, advances the intent to
`admitted`, and checks acknowledgement evidence. A request at a non-home HA node returns `503 Service Unavailable`;
there is no transport that sends its intent to the home for later finalization. OCI mutations do not use this admission
path.

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

## Shipped PyPI commit boundary

The request path commits blob, metadata, and replication-journal state before it waits for acknowledgement evidence,
then advances the intent to `admitted`. A `202 Accepted` response means publication committed but the evidence deadline
elapsed; a retry rechecks the same operation. Recording `published` follows a successful acknowledgement.

The local recovery sweep reads pending intents whose file rows are in the same store. Its finalizer validates the fence,
identity, placement, and current write grant, then commits the file rows, replication journal, operation outcome, and
intent advance in one transaction. It does not call the distributed acknowledgement resolver, so its placement check can
record `published` without the DC receipt evidence required by the request path. It cannot read rows or bytes from
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
