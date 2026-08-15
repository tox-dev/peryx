+++
title = "Finalizing admitted content"
description = "Publish admitted content at its authority home with fencing, validation, and idempotent outcomes."
weight = 8
aliases = [ "/core/availability-finalization/"]
+++

An ingress node can admit content before its authority home publishes it. Finalization runs at the
[home datacenter](@/core/availability/home-assignment.md). It validates the durable intent against current state, then
commits metadata, replication work, the operation outcome, and the intent transition in one transaction.

Content owners define admission requests and client responses. Finalization consumes owner-neutral intent, placement,
authorization, and operation contracts.

## States

The finalizer reads a durable ingress intent and moves it forward:

| State      | Meaning                                                                          |
| ---------- | -------------------------------------------------------------------------------- |
| `pending`  | The ingress accepted and staged the content; publication has not committed.      |
| `admitted` | Publication metadata, replication work, and the terminal outcome have committed. |

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

## Commit boundary

One local transaction writes the published metadata, replication journal entry, `published` outcome, and transition to
`admitted`. A crash cannot expose metadata without its replication record or leave a successful operation without the
outcome needed for replay.

This transaction also defines visibility. Pending or refused content remains absent from client views.

## Retry and restart

Finalization uses the operation ID as its idempotency key:

| Prior outcome | Result                                                                  |
| ------------- | ----------------------------------------------------------------------- |
| none          | Validate current state, then publish or refuse.                         |
| `published`   | Return the stored success without another publication or journal entry. |

A transient refusal can succeed after its cause clears because it creates no terminal outcome. A lost response or a
restart can trigger another attempt, but one operation ID can produce one committed publication.

## Placement

Finalization requires a verified placement somewhere in the topology. It does not wait for the content to reach the home
or each replica. Background replication and [remote read-through](@/core/remote-read-through.md) make those bytes
available after metadata publication.
