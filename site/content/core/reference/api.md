+++
title = "API explorer"
description = "Per-endpoint request and response breakdown with copyable examples, generated from the code."
weight = 3
template = "redoc.html"
+++

Each endpoint below lists its parameters, an example request, and responses by status code. `peryx openapi` generates
the document from the compiled public routes. Private availability-control and peer-replication routes use separate
contracts and do not appear here.

An operation's security requirements name the credentials its route accepts. A read takes `indexAccessToken`, an
`[[index.access_token]]` secret as the `Basic` password, or `bearerGrant`, a token the deployment minted; read authority
is enough for either, so no read asks for the write-granting `writeToken`. Server administration uses
`administratorPassword`.

A running server generates its own document at `/api-docs/openapi.json` from these routes and its own index ACLs. Where
every configured index allows anonymous reads, its read operations carry the empty requirement instead of the
credentials shown here, which is how `OpenAPI` spells anonymous access. The document above describes a deployment where
at least one index restricts reads, because that is the one whose contract names every credential.
