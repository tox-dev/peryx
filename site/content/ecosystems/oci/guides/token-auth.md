+++
title = "Make an OCI index private and issue tokens"
description = "Gate an OCI index's reads behind authentication, issue repository-scoped tokens with an expiry, and close a whole server's reads with one knob."
weight = 4
+++

By default an OCI index reads anonymously and authenticates only writes. This guide gates reads too, issues tokens
scoped to some repositories, and closes a whole server at once. Each section stands alone. The model is the neutral one
under [authentication and access control](@/core/authentication.md); this page is the OCI recipe.

## Turn on the realm

Set a signing key under `[auth]`. Without it peryx cannot mint tokens, so `docker login` does not validate and reads
stay open.

```toml
# peryx.toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
```

Keep the key in a file, not inline: a mounted Docker or Kubernetes secret, a systemd credential, or a Vault-rendered
file all fit `signing_key_file`. The key signs every token; rotating it invalidates all outstanding tokens.

## Gate an index's reads

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

## Scope a token to some repositories

`projects` is a list of globs. `*` matches any run of characters, `/` included, so `team/*` covers every repository
under `team` however deeply nested, and a bare `*` covers the whole index. Grant only the verbs the credential needs
under `actions` (`read`, `write`, `delete`).

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

An index may carry as many `[[index.access_token]]` tables as you need. The legacy `upload_token` shorthand still works
and stands for one credential that writes and deletes everywhere on the index.

## Give a token an expiry

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

## Close a whole server

To make every index private with one knob, set the server-wide default instead of touching each index:

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
default_anonymous_read = false
```

Each index's `anonymous_read` takes this default when it sets none, so one line closes reads everywhere; an index that
must stay public overrides it with `anonymous_read = true`.

## Control the token lifetime

`token_ttl_secs` under `[auth]` sets how long a minted token lives (default 300 seconds). A shorter lifetime makes a
revoked ACL take hold sooner; a longer one cuts token-endpoint traffic for a busy CI fleet.

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
token_ttl_secs = 900
```

## See also

- [Log in and push with a scoped token](@/ecosystems/oci/tutorials/scoped-token.md): the same setup as a walkthrough.
- [Control access to an index](@/core/control-access.md): the neutral cookbook, PyPI and OCI alike.
- [Token authentication](@/ecosystems/oci/reference/token-auth.md): the endpoints and error codes.
