+++
title = "Role grants"
description = "Manage the fixed role bindings that authorize server users."
weight = 9
+++

A role grant binds a server user to one fixed role over a reach: the whole server, or one named repository. The role
carries a constant set of scopes, so a grant persists only which role a user holds where rather than a hand-assembled
permission set that could drift. Peryx resolves every authorization decision against the live grants, so a change takes
effect on the next decision without a restart.

The four built-in roles are `administrator`, `repository_publisher`, `repository_reader`, and `operator`. See
[authentication and access control](@/core/access/authentication.md) for the scope each carries, and
[bootstrap the first administrator](@/core/access/bootstrap-administrator.md) for the account that seeds the model.

## Who may delegate

Only an administrator manages grants, and only within its reach. A server administrator delegates any role over any
repository and over server data; a repository administrator delegates only within its own repository. A publisher or
reader holds no delegation authority however broad its data access, so data access does not confer grant management. An
administrator's authority already covers every weaker role over its reach, so a delegated grant remains within the
caller's own authority.

## HTTP operations and authorization

The API uses local-user HTTP Basic authentication. Every route resolves the caller to a server user, loads its own
grants, and answers only what that caller may administer. A caller that cannot administer a reach cannot tell a grant
there apart from one that does not exist. Protected responses carry `Cache-Control: no-store`.

| Operation | Request                                                          |
| --------- | ---------------------------------------------------------------- |
| Grant     | `POST /+grants` with `{"user":"...","role":"...","scope":{...}}` |
| List      | `GET /+grants?user=...&resource=...&cursor=...&limit=25`         |
| Inspect   | `GET /+grants/{id}`                                              |
| Revoke    | `DELETE /+grants/{id}` with `If-Match: "<version>"`              |

A `scope` is `{"kind":"server"}` or `{"kind":"repository","name":"<name>"}`. A list `resource` filter is the reach in
path form, `server` or `repository/<name>`; a `user` filter selects one user's grants. Both filters need administration
authority over what they select, so a repository administrator may list its own repository but not the whole server. The
listing includes manual bindings and bindings provisioned by external identity links. Each grant carries a stable,
opaque `id`, a `version`, the granting actor and time, and an `origin`. Manual bindings report `{"kind":"manual"}`;
identity-owned bindings report `{"kind":"external_identity","link_id":"..."}`.

## Idempotency and conditional revocation

`POST /+grants` is idempotent. Creating an absent binding returns `201 Created`; re-asserting an existing one returns
`200 OK`, refreshes its actor and time, and advances its `version`. Every response carries that version as an `ETag`.

Revocation is conditional. `DELETE /+grants/{id}` requires an `If-Match` naming the version the caller observed. A
revoke that raced a re-assertion of the same binding sees a newer version and fails the precondition rather than
dropping the newer grant.

An identity-owned binding changes when its provider login refreshes group membership. A direct delete returns
`409 Conflict` with the owning link ID instead of pretending the binding does not exist.

| Outcome                               | Status                      |
| ------------------------------------- | --------------------------- |
| Removed                               | `204 No Content`            |
| Version differs from the precondition | `412 Precondition Failed`   |
| No `If-Match` presented               | `428 Precondition Required` |
| External identity owns the binding    | `409 Conflict`              |

Peryx rejects a grant to an unknown or disabled user, and an inert pairing whose role reaches nothing over its scope :
`operator` over a repository, for example, with `422 Unprocessable Entity`. A caller without authority over the target
reach receives `403 Forbidden` on a grant and `404 Not Found` on an inspect or revoke.

## Security events and timing

Each mutation emits one security event with the granting actor, the target user, the role, the reach, and the result.
The event carries no request body and no credential. The next authorization decision observes a revocation within the
serving decision's bound; see [digest revocations](@/core/repositories/digest-revocations.md) for that cache's timing.
