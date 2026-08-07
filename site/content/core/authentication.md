+++
title = "Authentication and access control"
description = "The neutral access model every ecosystem shares: principals, actions, project-glob grants, per-index tokens, and the anonymous-read policy."
weight = 10
+++

peryx uses one access model across packaging formats. It decides whether a principal may take an action on a resource in
an index. Ecosystem implementations map client credentials, resource names, and routes onto that model.

For task recipes see [control access to an index](@/core/control-access.md); for the reasoning behind the design see
[client auth versus upstream credentials](@/core/access-explained.md).

## The model

An access decision has four inputs.

A **principal** is who a request speaks as after peryx checks its credential. It is either anonymous or a named subject,
the name of the token that authenticated it. A credential that matches no token leaves the request anonymous, so an
invalid token is exactly as privileged as no token at all.

An **action** is one of `read`, `write`, and `delete`. The ecosystem implementation maps protocol operations onto these
actions.

A **grant** pairs a set of actions with a set of project globs. A token carries one grant, and a grant lets its actions
reach any project one of its globs matches.

An **index ACL** declares whether the index permits anonymous reads and which tokens it accepts. Each index has one, so
a cached index, a hosted store, and a virtual index all answer the same question.

## Where the model is enforced

Core resolves the index ACL and grant before an implementation performs a protected action. The implementation defines
its credential exchange, canonical resource identity, protected routes, and error response. Read coverage can differ by
implementation and route.

Supported access implementations:

- [PyPI](@/ecosystems/pypi/reference/endpoints.md)
- [OCI](@/ecosystems/oci/reference/token-auth.md)

`GET /+status` classifies version, coarse health, and the basic index list as public. Counters and implementation
rollups require `operator:read`; upstream hosts, access-token state, and recent writes require `administration:read`.
`GET /+stats` requires `operator:read` because it names repositories and resources. Discovery endpoints and aggregate
`GET /metrics` remain public. Restrict `/metrics` at the reverse proxy under the Prometheus security model. OpenID
Connect can add browser sign-in and a read-only web session. LDAP resolves server users for login consumers but does not
add an HTTP login route.

## Server roles and protected responses

Server users hold fixed roles over the whole server or one repository. A decision matches one named scope and one
resource against those grants. Repository reach cannot cover operator data, even when the role itself carries a server
scope. Separate role and resource checks follow the action-and-scope model used by
[Grafana RBAC](https://grafana.com/docs/grafana/latest/administration/roles-and-permissions/access-control/).

| Fixed role           | `repository:read` | `repository:write` | `repository:delete` | `operator:read` | `analytics:read` | `administration:read` |
| -------------------- | ----------------- | ------------------ | ------------------- | --------------- | ---------------- | --------------------- |
| Administrator        | yes               | yes                | yes                 | yes             | yes              | yes                   |
| Repository publisher | yes               | yes                | yes                 |                 |                  |                       |
|                      |                   |                    |                     |                 |                  |                       |
| Repository reader    | yes               |                    |                     |                 |                  |                       |
|                      |                   |                    |                     |                 |                  |                       |
|                      |                   |                    |                     |                 |                  |                       |
| Operator             |                   |                    |                     |                 |                  |                       |
|                      |                   |                    |                     |                 |                  |                       |
| yes                  | yes               |                    |                     |                 |                  |                       |
|                      |                   |                    |                     |                 |                  |                       |

| Field classification | Public caller | Repository caller | Operator caller | Administrator caller |
| -------------------- | ------------- | ----------------- | --------------- | -------------------- |
| Public               | yes           | yes               | yes             | yes                  |
| Repository           |               |                   |                 |                      |
| yes                  |               |                   |                 |                      |
| yes                  |               |                   |                 |                      |
| Operator             |               |                   |                 |                      |
| yes                  | yes           |                   |                 |                      |
| Administrator        |               |                   |                 |                      |
|                      |               |                   |                 |                      |
| yes                  |               |                   |                 |                      |

`operator:read` covers runtime health, queues, and configuration state. `analytics:read` covers retained usage
aggregates. The two scopes remain distinct, so each handler states which data family it reads. `administration:read`
covers server users, grants, credential state, and other data whose disclosure changes the attack surface. The
[Prometheus security model](https://prometheus.io/docs/operating/security/) treats operational and debug endpoints as
trusted-user data.

API and UI handlers classify each field as public, repository, operator, or administrator data, then filter a bounded
model before serialization. Repository and operator data remain separate; an administrator receives all four classes. A
nested object inherits the highest classification of its contents unless the handler filters that object first.

Role and field primitives do not authorize a route by themselves. Migrate a route in this order:

1. Resolve the server user and call `authorize_scoped` with the route's exact scope and resource before reading
   protected data.
1. Build a bounded response without holding metadata or request-path locks, and assign every field a classification.
1. Pass the scoped authorization result and classified fields to the shared response filter. The checked scope sets the
   caller's maximum field class; handlers cannot promote an operator decision to administrator access. Return the
   filter's generic denial without adding a resource path or query value.
1. Apply `private, no-cache` to caller-specific responses or `no-store` when a response contains credential or sensitive
   administration state.

Authorization failure returns no partial response. Peryx records the user, required scope, and bounded reason for a
denial; it omits the protected resource and raw query string.

Reverse proxies must preserve Peryx's `Cache-Control` header. The
[RFC 9111 `private` directive](https://www.rfc-editor.org/rfc/rfc9111.html#section-5.2.2.7) prevents a shared cache from
storing a caller-specific response while allowing a private cache to retain it; `no-cache` requires validation before
reuse. The [`no-store` directive](https://www.rfc-editor.org/rfc/rfc9111.html#section-5.2.2.5) applies to private and
shared caches. These directives constrain conforming caches and do not replace route authorization or TLS.

## Project globs

A grant's `projects` contains patterns that peryx matches against the implementation's canonical resource name. `*`
stands for any run of characters, including `/`; other characters match themselves.

| Pattern         | Matches                      | Does not match         |
| --------------- | ---------------------------- | ---------------------- |
| `*`             | every project in the index   |                        |
| `team-*`        | `team-widgets`, `team-tools` | `other-widgets`        |
| `team/*`        | `team/api`, `team/api/edge`  | `team`, `teamwork/api` |
| `acme-internal` | `acme-internal` only         | `acme-public`          |

Because `*` crosses `/`, `team/*` covers a resource subtree at any nesting depth. Supported identity rules:

- [PyPI](@/ecosystems/pypi/reference/policy.md)
- [OCI](@/ecosystems/oci/reference/policy.md)

## `[auth]`

The `[auth]` table holds the settings every index's access rules share. All keys are optional.

| Key                      | Meaning                                                                              | Default |
| ------------------------ | ------------------------------------------------------------------------------------ | ------- |
| `signing_key`            | Secret peryx signs its own tokens with                                               | (none)  |
| `signing_key_file`       | Path to read `signing_key` from instead of inlining it                               | (none)  |
| `token_ttl_secs`         | Lifetime of a minted token, in seconds; must be positive and at most 86400 (one day) | `300`   |
| `default_anonymous_read` | What an index's `anonymous_read` defaults to when the index omits it                 | `true`  |

`signing_key` and `token_ttl_secs` configure token-minting implementations. peryx reads the key at startup and uses it
to sign scoped tokens. Set at most one of `signing_key` and `signing_key_file`.

### LDAP providers

Each `[[auth.ldap_provider]]` names one StartTLS directory. Peryx constructs these providers at startup without opening
a connection; the first login performs the connection, TLS upgrade, search, and bind. peryx accepts only `ldap://` URLs
because each connection upgrades with StartTLS. A custom CA file extends the platform trust roots.

```toml
[[auth.ldap_provider]]
id = "corporate"
url = "ldap://directory.example:389"
base_dn = "ou=people,dc=example,dc=com"
mode = "service-search"
username_attribute = "uid"
bind_dn = "cn=peryx,ou=services,dc=example,dc=com"
bind_password_file = "/run/secrets/peryx-ldap-password"
subject_attribute = "entryUUID"
display_name_attribute = "displayName"
group_attribute = "memberOf"
ca_file = "/etc/peryx/directory-ca.pem"
connect_timeout_secs = 3
request_timeout_secs = 5
max_connections = 8

[[auth.ldap_provider.group_mapping]]
group = "cn=package-readers,ou=groups,dc=example,dc=com"
role = "repository_reader"
repository = "private"
```

`service-search` binds the configured service account, searches below `base_dn` for one exact `username_attribute`, then
binds that entry with the presented password. Set exactly one of `bind_password`, `bind_password_file`, or
`bind_password_env`. `direct-bind` needs `dn_attribute` instead of the service-account fields; it constructs
`{dn_attribute}=<escaped username>,{base_dn}` and binds that DN.

`subject_attribute` must be stable across renames. OpenLDAP's `entryUUID` and Active Directory's `objectGUID` fit;
email, username, and display name do not. `display_name_attribute` supplies the initial local name. `group_attribute` is
optional. When present, its exact values select `group_mapping` entries. A mapping without `repository` grants its role
at server scope; a mapping with `repository` must name a configured index.

peryx escapes LDAP filters and DN components before use. Searches return at most one entry and request the configured
attributes. `max_connections` is the total socket bound for the provider. peryx discards a socket that has carried a
user bind instead of returning it to the pool, including when cancellation or a timeout interrupts the login. Failed
credentials return no identity and cannot update the local user or managed grants. Directory and timeout failures remain
distinct errors without exposing the username, password, subject, groups, or CA contents.

The provider service returns a stable local user ID after the provider-subject link commits. It does not mint a token,
create a session, accept HTTP Basic credentials, or change a package route; those are separate consumers of the login
service.

### OIDC login providers

Each `[[auth.oidc_provider]]` configures one OpenID Connect issuer for browser sign-in through the Authorization Code
flow with PKCE. Peryx builds the provider at startup without a network call; the first login fetches and caches the
issuer's discovery document and signing keys and pins the configured issuer. The browser session cookie uses a key
derived from `[auth].signing_key`; startup rejects an `[[auth.oidc_provider]]` without a configured `signing_key`.

```toml
[[auth.oidc_provider]]
id = "corporate"
issuer = "https://idp.example/realms/main"
client_id = "peryx"
client_secret_file = "/run/secrets/peryx-oidc-secret"
redirect_uri = "https://artifacts.example/_/login/corporate/callback"
scopes = ["openid", "email", "groups"]
subject_claim = "sub"
display_name_claim = "name"
groups_claim = "groups"
clock_skew_secs = 30
request_timeout_secs = 8

[[auth.oidc_provider.group_mapping]]
group = "service-admins"
role = "administrator"

[[auth.oidc_provider.group_mapping]]
group = "packagers"
role = "repository_reader"
repository = "packages"
```

`issuer` and `redirect_uri` must be `https` and carry no fragment, and the issuer must carry no query. Register
`redirect_uri` verbatim with the provider; it is the `/_/login/{id}/callback` route peryx serves. Omit `client_secret`
(and its `_file` and `_env` siblings) for a public client that relies on PKCE alone, or set exactly one of them for a
confidential client. `scopes` always includes `openid`. `subject_claim` names the stable, opaque claim that identifies
the user; an email or a display name, which can be reassigned, does not belong here. `display_name_claim` supplies the
initial local name. `groups_claim` is optional; when present, its values select `group_mapping` entries the way LDAP
groups do. A mapping without `repository` grants a server-scoped role, while one with `repository` must name a
configured index.

**The login flow.** `GET /_/login/{id}` mints `state`, a `nonce`, and a PKCE verifier. It seals them into a single-use,
short-lived cookie and redirects the browser to the provider. The provider returns to `/_/login/{id}/callback`, where
peryx re-opens the sealed handoff and validates the response. The `state` must match, and the ID token must carry the
pinned issuer, this client's audience, a matching `nonce`, and a signature from the issuer's current keys, all within
`clock_skew_secs`. Any mismatch fails the login. A bounded metadata refresh picks up a rotated signing key.

**The session.** A completed login seals the resolved user into a short-lived `peryx_session` cookie with `HttpOnly`,
`Secure`, and `SameSite=Lax`, then redirects to the dashboard. The session authenticates the read-only web UI. A
state-changing request still authenticates with an `Authorization` header token. peryx does not accept the session
cookie as authorization for a mutation, which prevents a CSRF surface. `GET /_/session` reports the signed-in user and
configured providers for the login page. `POST /_/logout` clears the cookie.

**Outages.** `request_timeout_secs` bounds discovery, key, token, and user-info requests. Once a session exists, no
request reaches the provider. A provider outage fails an in-progress browser login with a retryable `503` but leaves
API-token authentication available, so protected operations continue while the identity provider is down. A
metadata-fetch failure retains cached keys and signature validation.

`default_anonymous_read = false` makes each index ACL deny anonymous reads by default. An implementation applies that
default to its protected routes. Public core routes stay open. An index that should stay open sets
`anonymous_read = true`.

## Per-index keys

These keys sit in an `[[index]]` table and are also listed under [configuration](@/core/configuration.md).

| Key              | Role | Meaning                                                  | Default                         |
| ---------------- | ---- | -------------------------------------------------------- | ------------------------------- |
| `anonymous_read` | all  | Whether a request with no credential may read this index | `[auth].default_anonymous_read` |

A hosted index accepts writes through `[[index.access_token]]` grants that permit the `write` action.

## `[[index.access_token]]`

Each `[[index.access_token]]` table adds one named credential the index accepts. Put these under the hosted index that
stores the writes.

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
| `secret`      | Shared secret an ecosystem authentication adapter verifies                           | (required) |
| `secret_file` | Path to read `secret` from instead of inlining it                                    | (none)     |
| `projects`    | Project globs the token may act on                                                   | `["*"]`    |
| `actions`     | Any of `read`, `write`, `delete`; at least one                                       | (required) |
| `expires_at`  | [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) time after which it stops working | never      |

A token needs exactly one of `secret` and `secret_file`. Once `expires_at` passes, the token authenticates nothing: a
request presenting it becomes anonymous, exactly as if the password were wrong.

## Secret files

Each secret key (`signing_key`, an access token's `secret`, and an LDAP `bind_password`) has a `_file` sibling naming a
path to read the value from, so no plaintext lives in the config file. peryx reads each file once at startup and trims
surrounding whitespace; an empty file is a startup error. The rationale and the tools it composes with are in
[client auth versus upstream credentials](@/core/access-explained.md).

## Server-user records

The metadata store can hold server users for management and later authentication features. A user receives a random,
opaque ID at creation. Renaming changes its display name and canonical lookup key without changing that ID. This follows
the [NIST subscriber-account model](https://pages.nist.gov/800-63-4/sp800-63a/accounts/), in which mutable account
attributes do not replace the stable subject identifier.

peryx trims display names for storage. Lookups compare an NFC-normalized lowercase key, so case changes and equivalent
composed Unicode spellings identify the same user. Creating or renaming to an existing canonical name fails without
changing either account. The original display spelling remains available for presentation.

New users are `active`. A disabled user remains inspectable by ID, but the next identity lookup no longer resolves it.
Reactivation restores lookup. Create, rename, disable, and reactivate operations append actor-neutral lifecycle records
in the same transaction as the account change. No operation in this lifecycle stores a password, token, role, or
external identity subject.

Opening an existing metadata store creates the user tables in one metadata transaction. Existing index configuration,
cached package records, and access policy remain in their current tables. If table initialization fails, the transaction
does not leave a partial user schema, and the prior metadata remains available to the existing recovery procedure.

Server users do not yet authorize package requests: mapping an authenticated user to grants waits on the role model.
Existing `[[index.access_token]]` credentials keep their current subjects and behavior when a server user is renamed or
disabled.

## External identity links

An external identity is the exact pair of a configured provider ID and that provider's opaque subject. Peryx preserves
the subject's spelling and case instead of substituting a mutable email address or display name. OpenID Connect defines
the [`iss` and `sub` pair](https://openid.net/specs/openid-connect-core-1_0.html#ClaimStability) as the stable
identifier; SCIM scopes each external identifier to its provisioning domain. Peryx creates distinct local users for
equal subjects from two providers.

The first successful provider login creates the local user and provider-subject link in the same metadata transaction.
Later logins resolve the same stable user ID. A colliding display name does not link accounts; peryx gives the new user
a distinct local display name. Linking by a matching email or display name would let one provider impersonate an account
created through another trust boundary.

Configured external groups map to the fixed server roles above. Peryx replaces the grants owned by that identity link
after each successful provider login, so removing a group takes effect on the next authorization decision. Peryx
preserves manual grants and grants owned by another link. The linker receives verified identities, so a failed provider
check cannot modify a link or its grants.

Diagnostics and security events omit provider subjects and group names. Link events contain the provider ID, local user
ID, result, and managed-grant count. Peryx persists the subject because the provider uses it as the stable lookup key.

The linking model is transport-neutral. OIDC and LDAP provide login transports; browser sessions and SCIM provisioning
remain separate integrations.

## Local password authentication

A server user may hold a local password. Enrollment derives a memory-hard verifier and discards the password, so the
plaintext is never written; only the verifier is stored, beside the account and keyed by its stable ID.

Verifiers are Argon2id, the algorithm [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106) standardizes, with the
[OWASP Password Storage](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html) baseline
parameters by default: 19 MiB of memory, two iterations, and a single lane over a random 128-bit salt. Each verifier
records the salt and parameters it was made with, so raising the policy does not invalidate the verifiers already
stored.

Authentication takes a display name and a password and returns the stable user ID on success. An unknown name, a
disabled account, an account with no password, and a wrong password all fail the same way: the same result and the same
cost. A login without a stored verifier still spends one derivation against a decoy, so a caller cannot tell an absent
account from a wrong password by watching how long the answer takes. An identity-store read that itself fails denies the
login rather than falling through to success.

A successful login whose verifier no longer matches the current policy re-enrolls it under the same user ID before
returning, so tightening the parameters upgrades verifiers as their owners sign in. A re-enrollment that cannot be
stored does not deny the login that already succeeded.

Each hash and check runs on the blocking pool because Argon2id uses 19 MiB per derivation. A semaphore caps concurrent
checks, which bounds login memory without starving request workers.

Passwords and verifiers are secrets end to end: neither appears in logs, errors, diagnostics, or any serialized account
view, and debug rendering redacts a verifier. Enrolling again replaces the verifier; clearing it removes password
authentication entirely. Clearing and then enrolling a new password is the recovery path when a local password is lost.
This release adds no self-service reset, password-reset email, or browser session.

## What this does not do

The model authorizes a client against peryx. It never sends a client's credential to an upstream: peryx reaches an
upstream with the stored per-index `username`, `password`, or `token` on the cached index, and a client's identity has
no bearing on that fetch. [Client auth versus upstream credentials](@/core/access-explained.md) explains why forwarding
would be unsafe for a cache.

## Related

- Supported implementations: [PyPI](@/ecosystems/pypi/reference/endpoints.md),
  [OCI](@/ecosystems/oci/reference/endpoints.md)
- Every other TOML key: [configuration](@/core/configuration.md)
- Security-event records for an authorization decision: [logging](@/core/logging.md)
