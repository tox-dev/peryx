+++
title = "API explorer"
description = "Per-endpoint request and response breakdown with copyable examples, generated from the code."
weight = 3
template = "redoc.html"
+++

Each endpoint below carries its parameters, an example request, and example responses per status code. The
`peryx openapi` subcommand generates this document from the source, so it cannot drift from the server; a running peryx
serves its own copy at `/api-docs/openapi.json`.
