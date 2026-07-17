+++
title = "Publish from CI identities"
description = "Exchange a GitHub Actions or GitLab CI OIDC identity for a short-lived, repository-scoped PyPI upload token."
weight = 6
+++

A CI provider signs an OIDC identity for one job. The job exchanges that identity at peryx, receives a short-lived Peryx
token, and gives the token to twine or `uv publish`. Peryx follows the
[PyPI Trusted Publishing protocol](https://docs.pypi.org/trusted-publishers/using-a-publisher/) and validates the
identity under [OpenID Connect Core](https://openid.net/specs/openid-connect-core-1_0.html#IDTokenValidation).

## Configure a publisher

Trusted publishing needs the signing secret for the token realm. Keep it in a mounted secret instead of the TOML file.
Set `repository` to the configured name of a writable PyPI hosted or virtual index. Peryx adds the URL route for that
index to each project grant, so the same project name on another route remains inaccessible.

{{ trusted_publishing_config() }}

Publisher IDs must be unique. `issuer`, `subject`, and each entry in `claims` must equal the identity value; `subject`
and `projects` use peryx globs. Peryx checks rules in file order; the first complete match determines the publisher ID
and grants. Keep the subject narrow and require stable numeric provider claims where they exist. Someone can delete and
reclaim a repository name; the `repository_id` and `repository_owner_id` fields from GitHub, or the `project_id` and
`namespace_id` fields from GitLab, survive that name-based ambiguity. On GitLab 18.4 or newer, prefer `job_project_id`
and `job_namespace_id`; the older fields identify the source project in a merge request pipeline. GitHub documents its
available fields in the [Actions OIDC claim reference](https://docs.github.com/en/actions/reference/security/oidc);
GitLab lists its fields in the [CI ID token reference](https://docs.gitlab.com/ci/secrets/id_token_authentication/).

Peryx mounts the exchange routes after an operator configures a publisher. Without a publisher it creates no OIDC client
or replay state. Peryx contacts an issuer during an exchange request.

## GitHub Actions

Limit `id-token: write` to the publishing job. Request the peryx audience, exchange the resulting identity, then pass
the Peryx token as the PyPI password. Do not enable shell tracing around these steps.

{{ trusted_publishing_github() }}

A user who can change a trusted workflow can publish as that workflow under common provider policies. Keep build steps
in a separate job, pin actions to reviewed commits, protect the release environment or tag, and avoid
`pull_request_target` in a publishing workflow. The PyPI
[trusted-publisher security model](https://github.com/pypi/warehouse/blob/main/docs/user/trusted-publishers/security-model.md)
explains why workflow control is equivalent to upload-token control.

GitHub repositories created after July 15, 2026 use an immutable default subject containing owner and repository IDs.
Repositories renamed or transferred after that date also switch formats. Match the `sub` value from a current identity
instead of copying the example based on names above. GitHub documents both forms under
[immutable subject claims](https://docs.github.com/en/actions/reference/security/oidc#immutable-subject-claims).

## GitLab CI

GitLab supplies the requested ID token through a job variable. Set its audience to the same value as
`[auth].oidc_audience`, and constrain the publisher with stable project and namespace IDs plus a protected ref or
environment.

{{ trusted_publishing_gitlab() }}

The exchange endpoint returns `{"token":"...","expires":<unix-seconds>}`. Twine and uv present that token as the
password for the exact username `__token__`; clients may also send it as `Authorization: Bearer`. A different Basic
username does not activate the short-lived-token path. The response forbids caching because it contains a bearer
credential. The signed token marks its trusted-publishing purpose. Peryx rejects ordinary OCI realm tokens on this path
and rejects trusted-publishing tokens at OCI endpoints, even when route names overlap.

## Verification and cache limits

Peryx obtains `/.well-known/openid-configuration` under the configured issuer path, requires the discovery document to
repeat that issuer byte for byte, and accepts RS256 signing keys that it discovers in the JWKS. It does not follow
redirects or use a JWT header or claim as a network location. Discovery requests have a five-second total timeout and a
64 KiB body limit; JWKS responses have the same timeout and a 1 MiB body limit. Peryx limits identity tokens to 32 KiB
and one hour from `iat` to `exp`, with no clock-skew allowance. It limits the subject to 2,048 bytes and the replay ID
to 256 bytes. The internal token expires at the earlier of `token_ttl_secs` and the external identity expiry.

The algorithm allowlist and issuer-to-JWK binding implement
[RFC 8725](https://www.rfc-editor.org/rfc/rfc8725.html#section-3). JWK metadata follows RFC 7517. Peryx accepts both
`application/json` and registered `application/*+json` responses. It treats the resulting upload credential as an RFC
6750 bearer token, even when a PyPI client wraps it in Basic `__token__` syntax.

Each issuer has an independent JWKS cache and refresh lock. Peryx clamps HTTP freshness from 60 through 900 seconds, and
a known JWK may remain usable during a transient refresh failure for at most one hour from the last successful fetch. An
unknown JWK or cold-cache failure triggers a refresh at most once per minute. Peryx keeps a working cache after a
duplicate JWK ID or malformed JWKS. An issuer outage does not block long-lived Basic uploads, minted Peryx tokens, or
exchanges through another issuer.

The cache reads `Cache-Control: max-age` under RFC 9111, then applies the documented min/max freshness and hard stale
limit. GitLab returns `max-age=0`; the one-minute floor prevents per-publish discovery and JWKS requests.

The stale-JWK window is an availability trade-off: removing a JWK at the issuer may take up to one hour to remove it
from a running Peryx process. Keep CI identity lifetimes short. For immediate revocation, remove the publisher from the
configuration and restart peryx; if the signing secret or a minted token may have leaked, rotate the realm secret too.
Changing that secret invalidates each outstanding Peryx realm token. Issuer JWK rotation needs no operator action.

Peryx consumes the `(issuer, jti)` pair from an identity after verification and authorization, so one process mints at
most one token from it. The atomic consume follows the Warehouse
[trusted-publisher replay handling](https://github.com/pypi/warehouse/blob/main/warehouse/oidc/services.py). The map
holds at most 65,536 live identities and is process-local; do not send one CI identity to several independent Peryx
processes. The replay cache does not coordinate across processes.

Security events contain the configured publisher ID and a random internal token ID. These IDs link mint and upload
events without logging credentials or unrestricted claims.
