+++
title = "Token authentication"
description = "The OCI Bearer token realm peryx serves: the /v2/ challenge, the /v2/token endpoint, the scope grammar, and the WWW-Authenticate error codes."
weight = 4
+++

peryx implements the [distribution token authentication](https://distribution.github.io/distribution/spec/auth/token/)
scheme so `docker login` validates a credential and a token can be scoped to some repositories. The access model behind
it (principals, actions, project-glob grants) is ecosystem-neutral and lives under
[authentication and access control](@/core/authentication.md); this page is the OCI wire surface that sits on top. For
the concept, read [why the token realm exists](@/ecosystems/oci/token-realm.md).

## Enabling the realm

The realm needs a signing key. Set `signing_key` (or `signing_key_file`) under `[auth]`; without it `GET /v2/` never
challenges, `GET /v2/token` answers `405`, and resource routes accept only Basic auth.

```toml
# peryx.toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
token_ttl_secs = 300                                # how long a minted token lives; default 300
default_anonymous_read = true                       # per-index anonymous_read default; default true
```

The key signs an HS256 JWT whose `aud` claim is `peryx`. Keep it secret and stable: rotating it invalidates every token
minted under the old key, and sharing it across replicas lets any replica verify a token the primary minted. Audience
validation prevents another service that shares the key from presenting its tokens to this registry.

## Version check

`GET /v2/` (with or without the trailing slash) answers one of two ways.

- `200` with `Docker-Distribution-API-Version: registry/2.0` when no OCI index restricts access, or when the request
  carries a credential the realm accepts (a bearer it signed, or a Basic password one of its indexes issued). This is
  the frictionless default and the `docker login` success signal.
- `401` with `WWW-Authenticate: Bearer realm="<base>/v2/token",service="peryx"` when an OCI index restricts access. An
  index restricts when its `anonymous_read` is `false` or it carries any named credential.

`<base>` is the origin peryx is reached at, read from the request's forwarded host. `service` is always `peryx`.

## The token endpoint

`GET /v2/token` mints a token. Query parameters:

| Parameter | Meaning                                                                                  |
| --------- | ---------------------------------------------------------------------------------------- |
| `service` | Required. The client echoes the challenge's service name, `peryx`.                       |
| `scope`   | An access request, `repository:<name>:<actions>`. Repeatable, or space-separated in one. |
| `account` | The username the client logged in as. Recorded for audit; not an input to authorization. |

Authentication:

- A missing, different, or repeated `service` gets `403`; peryx never mints a token for another audience.
- No `Authorization` header: the request is anonymous.
- `Basic` credentials: peryx checks the password against every OCI index's tokens. A password that authenticates nowhere
  gets `401`; this is what makes `docker login` reject a wrong password. A password that authenticates names its
  subject. A scope on another index is granted only when the same header authenticates as that subject there, so equal
  token names with different secrets remain isolated.

The response is always `200` on a recognized (or absent) credential, carrying a JWT:

```json
{
  "token": "<jwt>",
  "access_token": "<jwt>",
  "expires_in": 300
}
```

The token's granted access is the intersection of each requested scope with what the principal may do on the index the
`<name>` resolves to. An empty intersection is a valid token with no access, not an error: an anonymous request for a
public repository still gets a `pull` token, and one for a private repository gets a token that carries nothing.

## Scope grammar

A scope is `repository:<name>:<actions>`. `<name>` is the full `/v2/` repository name, index route prefix included
(`team/app`, `dockerhub/library/alpine`). `<actions>` is a comma-separated list; peryx maps each verb to a neutral
[action](@/core/authentication.md):

| Scope verb | Neutral action      | Granted for                |
| ---------- | ------------------- | -------------------------- |
| `pull`     | read                | `GET`/`HEAD` on a resource |
| `push`     | write               | `PUT`/`POST`/`PATCH`       |
| `delete`   | delete              | `DELETE`                   |
| `*`        | read, write, delete | any of the above           |

An unknown verb requests nothing. A scope whose resource type is not `repository`, whose name is empty, or that resolves
to no configured index is dropped from the request.

## Resource routes

Every `/v2/<name>/…` route authorizes the request against the index the name resolves to before its handler runs. It
accepts a Bearer JWT the realm signed, a Basic token (so an existing `docker login -u _ -p <token>` push keeps working),
or no credential (an anonymous read, when the index allows it). The HTTP method picks the action: `GET`/`HEAD` read,
`PUT`/`POST`/`PATCH` write, `DELETE` delete.

A refusal answers `401` with a scoped challenge:

```text
WWW-Authenticate: Bearer realm="<base>/v2/token",service="peryx",scope="repository:<name>:pull,push",error="insufficient_scope"
```

The `error` follows [RFC 6750](https://datatracker.ietf.org/doc/html/rfc6750#section-3.1):

| `error`              | Meaning                                                      | Client action                       |
| -------------------- | ------------------------------------------------------------ | ----------------------------------- |
| (none)               | No credential was presented on a route that needs one.       | Request a token, then retry.        |
| `invalid_token`      | The bearer failed signature, expiry, or audience validation. | Request a fresh token, then retry.  |
| `insufficient_scope` | The credential is valid but grants nothing for this action.  | Do not retry; the grant is missing. |

The `scope` names what the request needed, so a client can request the right token and retry. When no signing key is
configured, resource routes fall back to the Basic challenge (`WWW-Authenticate: Basic realm="peryx"`) instead.

## See also

- [Authentication and access control](@/core/authentication.md): the neutral model these routes enforce.
- [Client auth versus upstream credentials](@/core/access-explained.md): why a cached index never forwards a client's
  token to its upstream.
- [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md): the full `/v2/` route table.
