+++
title = "Availability behavior"
description = "How distribution clients observe replica refusal, fencing, retries, and readable frontiers."
weight = 8
+++

Distribution routes use the shared [availability contracts](@/core/availability-contracts.md).

- A read-only replica refuses blob and manifest mutations with `503 Service Unavailable` before protocol dispatch.
- A mutation under a superseded authority returns `503 Service Unavailable` with the `UNAVAILABLE` error code; retry it
  against the current writer.
- Blob and manifest retries converge by digest. A conflicting tag mutation remains a real conflict.
- Mutable tag reads remain behind the readable frontier; digest-addressed reads are served only from verified bytes.
- Cross-datacenter placement tracks each manifest and referenced blob independently.

Availability mode does not change the distribution protocol. It changes only when the server may acknowledge the
mutation. See [client behavior across availability modes](@/core/availability-client-behavior.md).
