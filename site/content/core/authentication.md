+++
title = "Authentication and access control"
description = "The neutral access model every ecosystem shares: principals, actions, project-glob grants, per-index tokens, and the anonymous-read policy."
weight = 10
+++

peryx answers one access question the same way for every packaging format: may this client take this action on this
project in this index. The model that answers it is ecosystem-neutral, so a PyPI upload and an OCI push run through the
same rules and differ only in how the client presents its credential. This page is the reference for that model and its
configuration keys.

For a walkthrough see [issue your first access token](@/core/first-token.md); for task recipes see
[control access to an index](@/core/control-access.md); for the reasoning behind the design see
[client auth versus upstream credentials](@/core/access-explained.md).

## The model

An access decision has four inputs.

A **principal** is who a request speaks as once its credential was checked. It is either anonymous or a named subject,
the name of the token that authenticated it. A credential that matches no token leaves the request anonymous, so an
invalid token is exactly as privileged as no token at all.

An **action** is one of `read`, `write`, and `delete`. Each ecosystem maps its verbs onto these: a pull or an install is
a read, a push or an upload is a write, and a removal is a delete.

A **grant** pairs a set of actions with a set of project globs. A token carries one grant, and a grant lets its actions
reach any project one of its globs matches.

An **index ACL** is what an index declares: whether anonymous reads are allowed, plus the tokens it accepts. Every index
has one, so a cached index, a hosted store, and a virtual index all answer the same question.

## Where the model is enforced

Write and delete authorization runs through the model in this release. A PyPI upload and a `docker push` present their
token as an HTTP Basic password, peryx resolves it to a principal against the target index's tokens, and the write
proceeds only if a grant covers the project and action.

OCI reads also run through the model. The registry challenges clients through its Bearer token realm, mints
repository-scoped tokens, and checks manifest, blob, tag, and catalog access. Server-rendered project pages, hydrated UI
requests, and search apply the same read ACLs, so they do not disclose inaccessible repositories.

PyPI's Simple API, JSON, metadata, and artifact routes do not consult read ACLs yet. The neutral discovery, status,
usage, and metrics endpoints also remain public. Setting `anonymous_read = false` does not protect those surfaces until
their handlers gain access checks. LDAP, token revocation, and per-mirror upstream credential refresh are also out of
this release. PyPI publishing can use a configured CI provider's OIDC identity without making OIDC a general login
source.

## Project globs

A grant's `projects` are patterns matched against a project or repository name. `*` stands for any run of characters,
including `/`, and every other character matches itself.

| Pattern         | Matches                      | Does not match         |
| --------------- | ---------------------------- | ---------------------- |
| `*`             | every project in the index   |                        |
| `team-*`        | `team-widgets`, `team-tools` | `other-widgets`        |
| `team/*`        | `team/api`, `team/api/edge`  | `team`, `teamwork/api` |
| `acme-internal` | `acme-internal` only         | `acme-public`          |

A PyPI project name is matched after [PEP 503](https://peps.python.org/pep-0503/) normalization; an OCI repository name
is matched as written. Because `*` crosses `/`, `team/*` covers a whole repository subtree however deeply nested.

## `[auth]`

The `[auth]` table holds the settings every index's access rules share. All keys are optional.

| Key                      | Meaning                                                              | Default |
| ------------------------ | -------------------------------------------------------------------- | ------- |
| `signing_key`            | Secret peryx signs its own tokens with                               | (none)  |
| `signing_key_file`       | Path to read `signing_key` from instead of inlining it               | (none)  |
| `token_ttl_secs`         | Lifetime of a minted token, in seconds; must be positive             | `300`   |
| `default_anonymous_read` | What an index's `anonymous_read` defaults to when the index omits it | `true`  |
| `oidc_audience`          | Audience external CI identity tokens must carry                      | `peryx` |

`signing_key` and `token_ttl_secs` configure the token realm used by OCI and PyPI trusted publishing. peryx reads the
key at startup and uses it to sign repository-scoped tokens. Set at most one of `signing_key` and `signing_key_file`.

Each `[[auth.trusted_publisher]]` binds one CI issuer, subject, required claim set, writable PyPI repository, and
project glob list. Peryx adds the exchange routes after an operator configures a binding. See
[publish from CI identities](@/ecosystems/pypi/guides/trusted-publishing.md) for the full table and provider examples.

`default_anonymous_read = false` makes every index's ACL deny anonymous reads by default. It closes the enforced OCI and
project-presentation paths; the public paths listed above stay open. An index that should stay open sets
`anonymous_read = true`.

## Per-index keys

These keys sit in an `[[index]]` table and are also listed under [configuration](@/core/configuration.md).

| Key                 | Role   | Meaning                                                         | Default                         |
| ------------------- | ------ | --------------------------------------------------------------- | ------------------------------- |
| `anonymous_read`    | all    | Whether a request with no credential may read this index        | `[auth].default_anonymous_read` |
| `upload_token`      | hosted | Sugar for one token granted write and delete over every project | (none)                          |
| `upload_token_file` | hosted | Path to read `upload_token` from instead of inlining it         | (none)                          |

`upload_token = "secret"` is shorthand for a single token named `upload_token` whose grant is write and delete over `*`,
which is the whole of peryx's access model before this release. It keeps working unchanged, so an existing config
behaves as it did. Set at most one of `upload_token` and `upload_token_file`.

## `[[index.access_token]]`

Each `[[index.access_token]]` table adds one named credential the index accepts, beyond the `upload_token` shorthand.
Put these under the hosted index that stores the writes.

```toml
[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
projects = ["team-*"]
actions = ["write", "delete"]
expires_at = "2027-01-01T00:00:00Z"
```

| Key           | Meaning                                                                              | Default    |
| ------------- | ------------------------------------------------------------------------------------ | ---------- |
| `name`        | Subject a request authenticating with this token speaks as; unique per index         | (required) |
| `secret`      | Password a client presents as its Basic password                                     | (required) |
| `secret_file` | Path to read `secret` from instead of inlining it                                    | (none)     |
| `projects`    | Project globs the token may act on                                                   | `["*"]`    |
| `actions`     | Any of `read`, `write`, `delete`; at least one                                       | (required) |
| `expires_at`  | [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) time after which it stops working | never      |

A token needs exactly one of `secret` and `secret_file`. `name` may not be `upload_token`, which is reserved for the
shorthand. Once `expires_at` passes, the token authenticates nothing: a request presenting it becomes anonymous, exactly
as if the password were wrong.

## Secret files

Every secret key (`signing_key`, `upload_token`, and a token's `secret`) has a `_file` sibling naming a path to read the
value from, so no plaintext lives in the config file. peryx reads each file once at startup and trims surrounding
whitespace; an empty file is a startup error. The rationale and the tools it composes with are in
[client auth versus upstream credentials](@/core/access-explained.md).

## Server-user records

The metadata store can hold server users for management and later authentication features. A user receives a random,
opaque ID at creation. Renaming changes its display name and canonical lookup key without changing that ID. This follows
the [NIST subscriber-account model](https://pages.nist.gov/800-63-4/sp800-63a/accounts/), in which mutable account
attributes do not replace the stable subject identifier.

Display names are trimmed for storage. Lookups compare an NFC-normalized lowercase key, so case changes and equivalent
composed Unicode spellings identify the same user. Creating or renaming to an existing canonical name fails without
changing either account. The original display spelling remains available for presentation.

New users are `active`. A disabled user remains inspectable by ID, but the next identity lookup no longer resolves it.
Reactivation restores lookup. Create, rename, disable, and reactivate operations append actor-neutral lifecycle records
in the same transaction as the account change. No operation in this lifecycle stores a password, token, role, or
external identity subject.

Opening an existing metadata store creates the user tables in one metadata transaction. Existing index configuration,
cached package records, and access policy remain in their current tables. If table initialization fails, the transaction
does not leave a partial user schema, and the prior metadata remains available to the existing recovery procedure.

Server users do not yet authenticate package requests. Existing `upload_token` and `[[index.access_token]]` credentials
keep their current subjects and behavior when a server user is renamed or disabled.

## What this does not do

The model authorizes a client against peryx. It never sends a client's credential to an upstream: peryx reaches an
upstream with the stored per-index `username`, `password`, or `token` on the cached index, and a client's identity has
no bearing on that fetch. [Client auth versus upstream credentials](@/core/access-explained.md) explains why forwarding
would be unsafe for a cache.

## Related

- The write endpoints each ecosystem exposes: [PyPI endpoints](@/ecosystems/pypi/reference/endpoints.md),
  [OCI endpoints](@/ecosystems/oci/reference/endpoints.md)
- Every other TOML key: [configuration](@/core/configuration.md)
- Security-event records for an authorization decision: [logging](@/core/logging.md)
