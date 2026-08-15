+++
title = "Client auth versus upstream credentials"
description = "Two separate questions peryx keeps apart: who a client is to peryx, and how peryx authenticates to an upstream. Why a cache must never forward the first to the second."
weight = 13
+++

peryx authenticates clients and authenticates itself to upstreams. Client authentication decides who may read or write
an index. Upstream authentication determines which credential peryx presents to a configured upstream. Mixing these
paths can expose cached private data.

## The two directions

A client authenticates **to peryx**. It presents a token, peryx resolves it to a principal, and a grant permits or
denies the action. The [access model](@/core/authentication.md) defines principals, actions, and resource-glob grants
per index.

peryx authenticates **to an upstream**. A cached index stores its own `username`, `password`, or `token`, which peryx
uses for each fetch through that index. The upstream sees peryx's service identity for all clients of the cached index.

The per-index upstream secret is `token`; the client credential is `access_token`. peryx sends the first to the upstream
and receives the second from a client.

## Why local rate limits use verified principals

`Authorization` claims an identity. The credential capability registered for the route's ecosystem decides whether to
accept it. Hashing the raw header before verification would let a client rotate invalid Basic or bearer values to obtain
a new rate-limit bucket for each value.

peryx asks that capability to verify the credential. After acceptance, peryx hashes the named principal with a
process-random seed. It groups invalid or anonymous traffic by client IP. The bounded bucket cache stores neither the
credential nor the principal name.

A caller can put any client address in a forwarding header, much as a caller can put an identity in `Authorization`.
Accepting that address without a trusted intermediary would let the caller rotate buckets. Behind a reverse proxy,
relying on the socket peer makes every anonymous client share the proxy's bucket.

peryx requires the socket peer to match `[rate_limit].trusted_proxies` before it accepts forwarding headers. It walks
`X-Forwarded-For` from the proxy end and selects the first address outside the trusted networks. Addresses closer to the
client came through an untrusted hop and cannot change the result. The same boundary gates `X-Forwarded-Host` and
`X-Forwarded-Proto`, which supply absolute discovery and authentication links. Without the check, a direct caller could
point clients or login flows at an origin it controls. If the trusted client-address suffix is malformed, peryx uses the
socket peer to avoid a forged address. It treats IPv4-mapped IPv6 addresses as their IPv4 equivalents.

[RFC 7239 section 8.1](https://www.rfc-editor.org/rfc/rfc7239.html#section-8.1) describes why forwarding fields need a
configured trust boundary. [Nginx's real-IP module](https://nginx.org/en/docs/http/ngx_http_realip_module.html) uses the
same nearest-untrusted-hop rule with recursive processing.

[RFC 9110 section 11.6.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-11.6.2) defines `Authorization` as
credentials that let a user agent authenticate. [Section 11.4](https://www.rfc-editor.org/rfc/rfc9110.html#section-11.4)
classifies invalid credentials as an authentication failure. Kubernetes
[API Priority and Fairness](https://kubernetes.io/docs/concepts/cluster-administration/flow-control/) follows the same
split. Authenticated flows can use the requesting user, while unauthenticated requests belong to
`system:unauthenticated`.

Enabling the limiter adds credential verification to requests that carry `Authorization`. Requests without that header
keep the existing route-classification path.

## Why peryx keeps client credentials local

Forwarding a client credential to the upstream would apply upstream access and rate limits to the cache miss. Cached
responses would bypass those checks.

A cache serves stored bytes to each authorized client. Suppose Alice's upstream credential fetched a private artifact;
peryx stores it. Bob then requests the same artifact and gets it from the cache without holding upstream access.
Forwarding preserves upstream access control for the first request but bypasses it for the cached response. Preserving
that control would require disabling the cache.

The upstream can also reject a token minted for the peryx audience. A client authenticates against a peryx-local name
such as `<route>/<resource>`, which can map to a different upstream name. The stored per-index credential supports an
authenticated upstream fetch without claiming that the client's peryx identity exists upstream.

## Where secrets live

peryx configuration can contain an upstream `password` or `token`, an index access token, and a signing key. Inline
values make the TOML secret material and unsuitable for version control.

Each secret key has a `_file` sibling naming a path to read. peryx reads the file once at startup and retains its value
in memory. This works with common secret-management mechanisms:

- Container orchestrators can mount secrets under `/run/secrets`, so an `[[index.access_token]]`
  `secret_file = "/run/secrets/hosted-token"` keeps plaintext out of the workload definition.
- systemd `LoadCredential` places a credential in a per-service directory, with optional TPM sealing, that a `_file` key
  points at.
- [Vault](https://developer.hashicorp.com/vault) Agent and [SOPS](https://github.com/getsops/sops) render a secret to a
  file that peryx reads the same way.

The `_file` indirection requires no cryptography inside peryx or another secret store. Inline values remain available
for local setup; deployments should use files.

## Related

- The keys and defaults: [authentication and access control](@/core/authentication.md)
- Point a secret at a file, task by task: [control access to an index](@/core/control-access.md)
- How the crates draw this boundary: [code architecture](@/contributing/architecture.md)
