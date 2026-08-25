+++
title = "Private OCI indexes and scoped tokens"
description = "Token signing, repository-scoped grants, expiry, catalog access, and server-wide anonymous-read policy."
weight = 4
+++

An OCI index allows anonymous reads and authenticates writes by default. The examples restrict reads, issue
repository-scoped tokens, and disable anonymous reads for the server. See
[authentication and access control](@/core/access/authentication.md) for the shared access model.

## Configure token signing

Set a signing key under `[auth]`. Without it peryx cannot mint tokens, so `docker login` does not validate and reads
stay open.

```toml
# peryx.toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
```

Keep the key in a file instead of inline. `signing_key_file` accepts a mounted Docker or Kubernetes secret, a systemd
credential, or a Vault-rendered file. Check the mounted file before starting peryx; an empty or whitespace-only key
stops startup. The key signs each token; rotating it invalidates all outstanding tokens.

## Restrict index reads

Set `anonymous_read = false` on the index and give a token a `read` grant. Now a pull needs a token that covers the
repository.

```toml
[[index]]
name = "team"
route = "team"
ecosystem = "oci"
hosted = true
anonymous_read = false

[[index.access_token]]
name = "ci"
secret_file = "/run/secrets/ci-token"
projects = ["team/*"]
actions = ["read", "write"]
```

A client pulls after `docker login localhost:4433 --username ci --password <ci-token>`. A pull of a repository the token
does not cover, or an anonymous pull, gets `401 insufficient_scope`.

The gate covers every web UI read; server rendering uses the incoming credential. After hydration, same-origin `/+ui`
and `/+search` requests apply the same ACL. Put the UI behind an authenticating proxy or send an `Authorization` header
to browse a private index. Search omits inaccessible repositories before calculating totals and pages.

## Scope a token

`projects` is a list of globs. `*` matches any run of characters, including `/`, so `team/*` covers repositories at any
depth under `team`, and a bare `*` covers the whole index. Grant the verbs the credential needs under `actions` (`read`,
`write`, `delete`).

```toml
[[index.access_token]]
name = "reader"
secret_file = "/run/secrets/reader-token"
projects = ["team/public/*"]
actions = ["read"]

[[index.access_token]]
name = "releaser"
secret_file = "/run/secrets/releaser-token"
projects = ["team/*"]
actions = ["read", "write", "delete"]
```

An index may define multiple `[[index.access_token]]` tables. One table with `actions = ["write", "delete"]` and no
`projects` filter is the credential that writes and deletes everywhere on the index.

## Set token expiry

Add `expires_at`, an RFC 3339 timestamp. After it passes, the token stops authenticating and a JWT already minted from
it stops verifying at its own expiry.

```toml
[[index.access_token]]
name = "ci-2027"
secret_file = "/run/secrets/ci-2027-token"
projects = ["team/*"]
actions = ["read", "write"]
expires_at = "2027-01-01T00:00:00Z"
```

## Allow catalog reads

`GET /v2/_catalog` spans the configured OCI indexes, so it uses `registry:catalog:*` instead of a repository pull scope.
Give the same named credential an explicit `projects = ["*"]` read grant on each private OCI index in the catalog. A
`team/*` grant authorizes matching pulls but cannot list the catalog.

Test the scope with these requests.

```shell
token=$(curl -sS -u ci:<ci-token> \
  'http://127.0.0.1:4433/v2/token?service=peryx&scope=registry%3Acatalog%3A%2A' | jq -r .token)
curl -sS --oauth2-bearer "$token" http://127.0.0.1:4433/v2/_catalog
```

peryx names `registry:catalog:*` in the `401` challenge for a missing credential. It returns `401 insufficient_scope`
for a valid token without that exact grant.

## Disable anonymous reads

Set the server-wide default to make indexes private unless they opt into anonymous reads:

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
default_anonymous_read = false
```

Each index inherits this value when it omits `anonymous_read`. A public index can override it with
`anonymous_read = true`.

## Set token lifetime

`token_ttl_secs` under `[auth]` sets how long a minted token lives (default 300 seconds). A shorter lifetime makes a
revoked ACL take hold sooner; a longer one cuts token-endpoint traffic for a busy CI fleet.

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
token_ttl_secs = 900
```

## See also

- [Log in and push with a scoped token](@/ecosystems/oci/tutorials/scoped-token.md): the same setup as a walkthrough.
- [Control access to an index](@/core/access/control-access.md): the neutral cookbook, PyPI and OCI alike.
- [Token authentication](@/ecosystems/oci/reference/token-auth.md): the endpoints and error codes.
