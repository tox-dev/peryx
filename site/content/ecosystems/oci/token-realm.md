+++
title = "Why peryx is a Bearer token realm"
description = "How the OCI token flow works, why an anonymous token can pull a public repository, and why turning on the challenge does not break anonymous pulls."
weight = 4
+++

peryx answers `GET /v2/` with a `401` and a `WWW-Authenticate: Bearer` challenge, the same shape Docker Hub uses. This
page explains why the token flow is built this way, and why the change that makes `docker login` honest does not cost
you anonymous pulls. It is about the auth peryx runs against its own clients; the separate question of how peryx
authenticates to an upstream is [Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md), and the
line between the two is drawn in [client auth versus upstream credentials](@/core/access-explained.md).

## The problem it fixes

peryx used to answer `GET /v2/` with `200` and no challenge, and enforce auth only on writes, as a Basic check against
one static upload token. Two things followed. `docker login` succeeded with any password, because the daemon reads a
`200` on the retried probe as proof the credentials are good, and there was nothing to make that probe fail. And a wrong
token surfaced only at push time as a `denied`, never at login, where a person would notice.

The Basic scheme cannot do better. It has no anonymous grant and no scope, so it can express "everyone authenticates" or
"nobody does", but not "anonymous may read, an authenticated client may write". Closing the login gap with Basic would
have forced every anonymous puller to authenticate.

## The flow

The Bearer scheme splits authentication into a handshake with a dedicated token endpoint. `docker login localhost:4433`
runs it:

1. `GET /v2/` returns `401` with `WWW-Authenticate: Bearer realm=".../v2/token",service="peryx"`. The daemon learns
   where to authenticate.
1. `GET /v2/token` with the Basic credentials. peryx checks the password against the index tokens and returns a signed
   JWT. A password that authenticates nowhere gets a `401`, and the login fails there.
1. `GET /v2/` again, now with `Authorization: Bearer <jwt>`. peryx verifies the token and answers `200`, which the
   daemon reads as login success.

A pull or push adds a fourth move: the daemon requests a token scoped to the repository it is about to touch
(`repository:team/app:pull,push`), and presents it on the resource route. peryx verifies the token, resolves the name to
an index, and authorizes the action against that index's live rules.

## Why an anonymous token pulls a public repository

The token endpoint authenticates the client first, which may be an anonymous client that sent no credential at all. Then
it decides access per scope: it grants the actions the principal may take on the repository the scope names. For a
public repository, an anonymous principal may read, so the token carries `pull`. For a private one, the anonymous
principal may not, so the same request yields a token with no access.

Public versus private is therefore a decision the token endpoint makes per scope at issue time, not a property of the
`/v2/` endpoint. The registry only has to emit an accurate scope in each challenge and enforce the token's access on the
resource route. This is what the distribution spec means when it says an unauthenticated client still gets a token, and
that a client with access to only a subset of the requested scope is not an error.

## Why the challenge does not break anonymous pulls

The worry is that a blanket `401` on `/v2/` forces every client, logged in or not, to authenticate. For Bearer it does
not, because the anonymous path runs the same three moves and ends in a `200`: an anonymous client requests a token with
no credentials, receives one carrying whatever the index grants anonymously, and pulls with it. `docker pull` of a
public image never runs `docker login` and never has to.

peryx keeps the frictionless default besides. It challenges `GET /v2/` only when an OCI index actually restricts access,
by gating its reads or carrying a named credential. A zero-config deployment, where every index reads anonymously and no
token is configured, still answers `200` with no challenge, so nothing about the default install changes. And a resource
push still accepts a Basic token for this release, so an existing `docker login -u _ -p <token>` flow keeps working
while clients move to the token endpoint.

## Why the tokens are JWTs

A peryx token is a self-contained HS256 JWT: the client's credential is an expiring, signed assertion of the grants the
token endpoint approved. Verifying one is a signature check with no database lookup, so a replica can verify a token the
primary minted without sharing state, which is the property the high-availability story needs. The signing key is
identity state, not protocol state: the OCI crate calls the neutral signer to mint and verify, and never sees the key.

## See also

- [Authentication and access control](@/core/authentication.md): the neutral model the realm enforces.
- [Token authentication](@/ecosystems/oci/reference/token-auth.md): the endpoints, scope grammar, and error codes.
- [Log in and push with a scoped token](@/ecosystems/oci/tutorials/scoped-token.md): the flow, hands on.
