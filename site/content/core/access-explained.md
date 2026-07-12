+++
title = "Client auth versus upstream credentials"
description = "Two separate questions peryx keeps apart: who a client is to peryx, and how peryx authenticates to an upstream. Why a cache must never forward the first to the second."
weight = 13
+++

peryx handles two authentication questions that look alike and must stay apart. One faces the client: who are you, and
may you read or write this index. The other faces the upstream: what credential does peryx present to
[pypi.org](https://pypi.org/) or a private registry to fetch on your behalf. Conflating them is the mistake this design
exists to avoid.

## The two directions

A client authenticates **to peryx**. It presents a token, peryx resolves it to a principal, and a grant decides whether
the action is allowed. This is the [access model](@/core/authentication.md): principals, actions, and project-glob
grants, configured per index.

peryx authenticates **to an upstream**. A cached index carries its own stored `username`, `password`, or `token`, and
peryx uses that one service credential for every fetch through that index. The client's identity plays no part. Whoever
installs through a cached index reaches the upstream as peryx, not as themselves.

These are different credentials pointing in opposite directions. Keeping them separate is why the per-index upstream
secret is `token` while a client credential is an `access_token`: one is what peryx sends out, the other is what a
client sends in.

## Why local rate limits use verified principals

`Authorization` claims an identity. The driver for the route decides whether to accept it. Hashing the raw header before
that check lets a client rotate invalid Basic or bearer values and receive a new rate-limit bucket for each value.

peryx asks the driver that owns the route to verify the credential; after acceptance, peryx hashes the named principal
with a process-random seed and groups invalid or anonymous traffic by client IP, storing neither the credential nor the
principal name in the bounded bucket cache.

[RFC 9110 section 11.6.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-11.6.2) defines `Authorization` as
credentials that let a user agent authenticate. [Section 11.4](https://www.rfc-editor.org/rfc/rfc9110.html#section-11.4)
classifies invalid credentials as an authentication failure. Kubernetes
[API Priority and Fairness](https://kubernetes.io/docs/concepts/cluster-administration/flow-control/) follows the same
split. Authenticated flows can use the requesting user, while unauthenticated requests belong to
`system:unauthenticated`.

Operators who enable the limiter pay credential verification on requests that carry `Authorization`. peryx keeps the
existing route-classification path for requests without that header.

## Why peryx never forwards a client's credential upstream

A tempting shortcut is to take the credential a client presented and replay it against the upstream, so a private
upstream repository or a rate limit keyed to the client's identity carries through. peryx does not do this, and a cache
is the reason.

A cache serves stored bytes to every authorized client. Suppose Alice's upstream credential fetched a private layer;
peryx stores it. Bob then requests the same layer and gets it from the cache, having never held any upstream access.
Forwarding looks like it preserves the upstream's access control, and it silently destroys it on the second request. The
only way to preserve it would be to disable caching, which deletes the reason peryx exists.

Two smaller problems compound the first. A token peryx minted for its own audience would be rejected upstream, so
forwarding is not even mechanically free. And the client authenticates against a peryx-local name
(`root/oci/library/postgres`) that only peryx knows maps to `library/postgres` upstream, so its peryx identity says
nothing about its upstream one. The legitimate need behind forwarding, authenticated upstream pulls for a private
repository or a rate limit, is served by the stored per-index credential instead.

## Where secrets live

peryx needs several secrets in its configuration: the upstream `password` or `token`, a hosted index's upload token, and
the signing key for its own token realm. Writing them inline in the TOML works and stays supported, but it is the lesser
option: the file becomes secret material, so it cannot sit in version control or a config-management repository without
care.

Every secret key therefore has a `_file` sibling naming a path to read the value from. peryx reads the file once at
startup and holds only the value, never the path's contents on disk beyond that read. This composes with the mechanisms
a deployment already uses for secrets:

- Docker and Kubernetes mount secrets as files under `/run/secrets`, so
  `upload_token_file = "/run/secrets/hosted-token"` reads a Kubernetes `Secret` with no plaintext in the manifest.
- systemd `LoadCredential` places a credential in a per-service directory, optionally sealed to the TPM, that a `_file`
  key points at.
- [Vault](https://developer.hashicorp.com/vault) Agent and [SOPS](https://github.com/getsops/sops) render a secret to a
  file that peryx reads the same way.

The `_file` indirection needs no crypto inside peryx and no new secret store; it hands the job to the tool that already
owns your secrets. Inline values remain for a quick local setup, documented as the option to move off of.

## Related

- The keys and defaults: [authentication and access control](@/core/authentication.md)
- Point a secret at a file, task by task: [control access to an index](@/core/control-access.md)
- How the crates draw this boundary: [code architecture](@/contributing/architecture.md)
