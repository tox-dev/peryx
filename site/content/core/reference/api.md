+++
title = "API explorer"
description = "Per-endpoint request and response breakdown with copyable examples, generated from the code."
weight = 3
template = "redoc.html"
+++

Each endpoint below lists its parameters, an example request, and responses by status code. `peryx openapi` generates
the document from the compiled public routes. A running server exposes the same schema at `/api-docs/openapi.json`.
Private availability-control and peer-replication routes use separate contracts and do not appear here.
