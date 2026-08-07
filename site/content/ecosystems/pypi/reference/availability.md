+++
title = "Availability behavior"
description = "How Python package clients observe replica refusal, fencing, retries, and readable frontiers."
weight = 8
+++

Python package routes use the shared [availability contracts](@/core/availability-contracts.md).

- A read-only replica refuses uploads and other mutations with `503 Service Unavailable` before reading the upload.
- A publish whose authority epoch was superseded returns `409 Conflict`; retry the unchanged request against the current
  writer.
- Repeating the same upload bytes is idempotent by digest and resolves to the original result.
- A hosted or cached project page remains behind the readable frontier until its derived view catches up.
- Ingress saturation returns `503 Service Unavailable` with `Retry-After` rather than retaining unbounded uploads.

Availability mode does not change the Simple API or upload format. It changes only when the server may acknowledge the
mutation. See [client behavior across availability modes](@/core/availability-client-behavior.md).
