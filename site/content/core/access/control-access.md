+++
title = "Control access to an index"
description = "Recipes for the common access tasks: scope a token to some resources, declare an index's reads private, close a whole server, and keep a secret in a file."
weight = 11
+++

These recipes use the model in [authentication and access control](@/core/access/authentication.md). Ecosystem owners
map their client credentials and routes onto the shared actions.

## Scope a token to some resources

Add an `[[index.access_token]]` table to the hosted index. `resources` is a list of globs; `actions` is any of `read`,
`write`, and `delete`.

```toml
[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
resources = ["team-*", "shared/tools"]
actions = ["write"]
```

The client presents the secret through its ecosystem authentication flow. The token may write a resource matching
`team-*` or the exact name `shared/tools`. A write to another name returns the implementation's authorization denial.
Give a token `actions = ["write", "delete"]` when the same credential may remove resources. An index can carry several
`[[index.access_token]]` tables; each needs a distinct `name`.

Supported client flows:

- [Ecosystem owner documentation](@/ecosystems/_index.md)

## Let one token write everywhere

For a hosted index that a single trusted credential may write and delete across every resource, one
`[[index.access_token]]` grant with no `resources` filter is the whole configuration:

```toml
[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "writer"
secret = "hosted-secret"
actions = ["write", "delete"]
```

An omitted `resources` filter defaults to `*`, so this one token writes and deletes everywhere. Add a `resources` list
the moment you need per-resource scope.

## Declare an index's reads private

By default any client may read an index. Set `anonymous_read = false` to require a credential to read it:

```toml
[[index]]
name = "internal"
hosted = true
anonymous_read = false

[[index.access_token]]
name = "reader"
secret = "reader-secret"
resources = ["*"]
actions = ["read"]
```

The flag denies anonymous reads on routes the selected owner protects. A token with the `read` action names the
resources its principal may read. See the supported client flows above for route coverage.

Leaving `anonymous_read = true` opens artifact reads to callers that present nothing; it never widens what a presented
credential may reach. The operational endpoints - `/+query`, `/+analytics`, and `/+quota` - answer a repository token
only when its password resolves to a live token granting `read` over `*`, and answer `401` otherwise.

## Close a whole server

Setting `anonymous_read = false` on every index is tedious and easy to forget on a new one. The `[auth]` table flips the
default instead:

```toml
[auth]
default_anonymous_read = false
```

Every index now defaults to private reads, and an index that should stay open opts back in with `anonymous_read = true`.
One knob makes a fully private server the default and a public index the exception.

## Rate-limit named principals

Enable local rate limits when authenticated clients need buckets separate from callers sharing their IP address. peryx
verifies a presented credential through the capability registered for the route's ecosystem. Credentials resolving to
the same named principal share one bucket per route class, including after a Basic username or bearer change. Invalid
and anonymous credentials share the source-address bucket.

Route groups and their configuration keys belong to the selected ecosystem. See the
[ecosystem documentation](@/ecosystems/_index.md) for its rate-limit example.

## Preserve client buckets behind a reverse proxy

List the networks from which peryx accepts proxy connections:

```toml
[rate_limit]
enabled = true
trusted_proxies = ["127.0.0.1/32", "10.42.0.0/16"]
```

Add proxy addresses and exclude client networks. The edge proxy must replace caller-supplied `X-Forwarded-For`,
`X-Forwarded-Host`, and `X-Forwarded-Proto`; each later trusted proxy appends its own peer. Peryx starts at the socket
peer and selects the nearest address outside the configured networks. It uses the forwarded host and protocol in public
links. Requests from other peers use the request URI or `Host`; a relative URI defaults to HTTP. This check applies even
when the rate limiter is disabled. Peryx uses the socket peer when a trusted client-address suffix contains malformed
input.

Leave `trusted_proxies` empty when clients connect to peryx without a proxy. See
[serve HTTPS](@/core/operations/serve-https.md#terminate-tls-at-a-reverse-proxy) for an nginx configuration that
overwrites the client-controlled headers.

## Keep a secret out of the config file

Every secret key has a `_file` sibling that names a path to read the value from, so the config file holds no plaintext:

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"

[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "writer"
secret_file = "/run/secrets/hosted-token"
actions = ["write", "delete"]

[[index.access_token]]
name = "ci"
secret_file = "/run/secrets/ci-token"
resources = ["team-*"]
actions = ["write"]
```

peryx reads each file once at startup and trims trailing whitespace, so a file written by `echo` or mounted by an
orchestrator works unchanged. An empty file is a startup error. Set a key or its `_file` sibling, never both. This
composes with secret mounts under `/run/secrets`, systemd `LoadCredential`, and files rendered by
[Vault](https://developer.hashicorp.com/vault) or [SOPS](https://github.com/getsops/sops), covered in
[client auth versus upstream credentials](@/core/access/access-explained.md).

## Related

- The full model and every key: [authentication and access control](@/core/access/authentication.md)
- [Ecosystem owner documentation](@/ecosystems/_index.md)
- The `[auth]` and `[[index.access_token]]` keys in context: [configuration](@/core/operations/configuration.md)
