+++
title = "OCI token realm"
description = "Why peryx issues Distribution Bearer tokens and how repository scopes map to index access."
weight = 6
+++

Docker treats a saved credential as valid only after the registry accepts an authenticated `GET /v2/`. A Basic-only
registry cannot express anonymous pulls with authenticated pushes during that probe. The OCI implementation supports the
Distribution Bearer flow.

## Activation

Set `[auth].signing_key` or `[auth].signing_key_file` to enable `/v2/token`. The OCI implementation asks
`peryx-identity` to issue and verify HS256 tokens; it does not read key material. Startup rejects an empty key.

An OCI index with restricted reads or a named credential challenges `GET /v2/`. Public OCI indexes without credentials
answer the probe directly and do not require token exchange.

## Exchange

1. The registry answers `GET /v2/` with a Bearer challenge naming `/v2/token` and service `peryx`.
1. The client calls `/v2/token` with Basic credentials and an optional Distribution scope.
1. The realm returns a short-lived token containing the actions that credential may perform.
1. The client retries the registry request with the Bearer token.

Repository scopes use `repository:<name>:pull,push`. The OCI implementation maps `pull` to read access and `push` to
write and delete access, then evaluates the selected index's current grants. Anonymous callers can receive pull scope
for public repositories.

`GET /v2/_catalog` uses `registry:catalog:*`. A credential must have an explicit `projects = ["*"]` read grant on each
private OCI index included in the catalog; a repository-specific token cannot enumerate it.

## Details

- [Token authentication reference](reference/token-auth.md) defines claims, scopes, challenges, and errors.
- [Make an OCI index private](guides/token-auth.md) covers key storage, credentials, rotation, and catalog grants.
- [Scoped token tutorial](tutorials/scoped-token.md) exercises login, allowed push, denial, and catalog access.
