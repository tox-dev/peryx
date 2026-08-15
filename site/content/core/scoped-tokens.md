+++
title = "Scoped token lifecycle"
description = "Create, list, inspect, rotate, and revoke named API tokens over a reach the caller is authorized to grant."
weight = 12
+++

A scoped token is a named, expiring credential an authorized user mints over a reach: the whole server, or one named
repository. It is the managed complement to config-only `[[index.token]]` credentials, created, rotated, and revoked
through the live API instead of a configuration file, with each change recorded and its secret shown only once.

The token carries a set of actions (`read`, `write`, `delete`) over its reach, the same vocabulary an index ACL grant
speaks. Peryx stores only a SHA-256 verifier of the secret, never the secret itself, so a leaked metadata store
discloses no usable credential.

## Authority: you can only grant what you hold

Creating a token validates its reach against the caller's own role grants. A server-wide token, with no repository :
requires administrator authority. A repository-scoped token requires the caller's authority for each requested action on
that repository. So a repository manager can mint a token for their own repository but cannot mint a server-wide or
cross-repository one; the request answers `404`, disclosing neither the repository nor the token.

Listing, inspecting, rotating, and revoking a token require write authority over its reach: repository write for a
repository token, administrator authority for a server token. A repository reader can mint a read-only token for its
repository but cannot manage tokens.

## HTTP operations

The API uses local-user HTTP Basic authentication. Every response sets `Cache-Control: no-store`.

| Operation | Request                                                               |
| --------- | --------------------------------------------------------------------- |
| Create    | `POST /+tokens` with `{"name","repository?","actions","expires_at?"}` |
| List      | `GET /+tokens?repository=&cursor=&limit=`                             |
| Inspect   | `GET /+tokens/{id}`                                                   |
| Rotate    | `POST /+tokens/{id}/rotate`                                           |
| Revoke    | `DELETE /+tokens/{id}`                                                |

Omit `repository` for a server-wide token; name an index route to scope the token to it. `actions` must list at least
one of `read`, `write`, `delete`. An `expires_at`, when given, is a Unix timestamp in the future. The list is bounded
and cursor-paginated within one reach, at most 100 rows per page.

```console
$ admin=(--user admin --password-file /run/secrets/peryx-admin-password)
$ curl -su admin:@- https://artifacts.example/+tokens \
    -H 'content-type: application/json' \
    -d '{"name":"ci-writer","repository":"hosted","actions":["read","write"],"expires_at":1800600000}'
{"token":{"id":"tok_...","name":"ci-writer","reach":{"kind":"repository","name":"hosted"},...},"secret":"peryx_..."}
```

## The secret, expiry, and revocation

The `POST /+tokens` and `POST /+tokens/{id}/rotate` responses reveal the secret once, under `secret`; every later read
returns the token's metadata without it. Store the value when it is shown. Rotation issues a new secret and invalidates
the prior one, leaving the token's id, reach, and actions unchanged; a failed rotation leaves the prior secret valid,
and a revoked token cannot be rotated.

Verification resolves a presented secret to its token with one indexed read and no database write, so authorizing a
request never writes. Revocation removes that index entry, so a revoked token stops authenticating on its next request
while its record and lifecycle evidence remain for an administrator to inspect. Revoking one token leaves every other
token valid. Revocation is idempotent: revoking an already-revoked token returns its unchanged record.

Each create, rotate, and revoke emits one security event carrying the actor and the stable token id, never the secret or
its verifier.

## Migrating a configured index token

A hosted index's `[[index.access_token]]` entries keep working unchanged. To move one to a managed token, mint a scoped
token for that repository with the same actions, distribute its one-time secret to the client, then remove the
configured entry. Because a managed token is revoked and rotated through the API, an operator no longer edits
configuration or restarts the server to retire a credential.
