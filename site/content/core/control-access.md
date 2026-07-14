+++
title = "Control access to an index"
description = "Recipes for the common access tasks: scope a token to some projects, declare an index's reads private, close a whole server, and keep a secret in a file."
weight = 11
+++

Each task below is self-contained. They share one model, described in full under
[authentication and access control](@/core/authentication.md); this page is the cookbook.

Write and delete authorization runs through the model today, enforced as HTTP Basic auth on a PyPI upload and on a
`docker push`. The read side (`anonymous_read`) is recorded now and enforced when the read challenge ships, so you can
declare the policy ahead of the enforcement.

## Scope a token to some projects

Add an `[[index.access_token]]` table to the hosted index. `projects` is a list of globs; `actions` is any of `read`,
`write`, and `delete`.

```toml
[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
projects = ["team-*", "shared/tools"]
actions = ["write"]
```

A client presents the secret as its Basic password (`-u __token__ -p ci-secret` for twine, `-p ci-secret` for
`docker login`). The token may write any project matching `team-*` or the exact name `shared/tools`, and nothing else. A
write to another name returns `403`. Give a token `actions = ["write", "delete"]` if the same credential should also
remove releases. An index can carry several `[[index.access_token]]` tables; each needs a distinct `name`.

## Let one token write everywhere

For a hosted index that a single trusted credential may write and delete across every project, the `upload_token` key is
the whole configuration:

```toml
[[index]]
name = "hosted"
upload_token = "hosted-secret"
```

This is sugar for one token granted write and delete over `*`. Use it when you do not need per-project scope; reach for
`[[index.access_token]]` the moment you do.

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
projects = ["*"]
actions = ["read"]
```

The flag records that this index's reads are not open, and a read-granting token expresses who may still read it. Read
enforcement arrives with the read challenge; until then the flag is recorded but reads are served openly, so treat this
as declaring the policy ahead of the gate.

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

Enable local rate limits when authenticated clients need buckets separate from callers sharing their IP address.

```toml
[rate_limit]
enabled = true
max_clients = 4096

[rate_limit.upload]
requests = 30
window_secs = 60
```

peryx verifies a presented credential through the driver that owns the route. Credentials resolving to the same named
principal share one bucket per route class, including after a Basic username or bearer change. Invalid and anonymous
credentials share the source-address bucket. A client cannot allocate fresh buckets by rotating invalid credentials.

## Preserve client buckets behind a reverse proxy

List the networks from which peryx accepts proxy connections:

```toml
[rate_limit]
enabled = true
trusted_proxies = ["127.0.0.1/32", "10.42.0.0/16"]
```

Add proxy addresses and exclude client networks. The edge proxy must replace caller-supplied `X-Forwarded-For`; each
later trusted proxy appends its own peer. Peryx starts at the socket peer and selects the nearest address outside the
configured networks. It ignores forwarding headers from direct callers and uses the socket peer when a trusted suffix
contains malformed input.

Leave `trusted_proxies` empty when clients connect to peryx without a proxy. See
[serve HTTPS](@/core/serve-https.md#terminate-tls-at-a-reverse-proxy) for an nginx configuration that overwrites the
client-controlled headers.

## Keep a secret out of the config file

Every secret key has a `_file` sibling that names a path to read the value from, so the config file holds no plaintext:

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"

[[index]]
name = "hosted"
upload_token_file = "/run/secrets/hosted-token"

[[index.access_token]]
name = "ci"
secret_file = "/run/secrets/ci-token"
projects = ["team-*"]
actions = ["write"]
```

peryx reads each file once at startup and trims trailing whitespace, so a file written by `echo` or mounted by an
orchestrator works unchanged. An empty file is a startup error. Set a key or its `_file` sibling, never both. This
composes with Docker and Kubernetes secret mounts under `/run/secrets`, systemd `LoadCredential`, and files rendered by
[Vault](https://developer.hashicorp.com/vault) or [SOPS](https://github.com/getsops/sops), covered in
[client auth versus upstream credentials](@/core/access-explained.md).

## Related

- The full model and every key: [authentication and access control](@/core/authentication.md)
- A start-to-finish walkthrough: [issue your first access token](@/core/first-token.md)
- The `[auth]` and `[[index.access_token]]` keys in context: [configuration](@/core/configuration.md)
