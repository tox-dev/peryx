+++
title = "Configuration"
description = "Shared TOML keys, flags, defaults, and precedence. Owner settings remain in ecosystem documentation."
weight = 1
+++

Release binaries contain the shipped ecosystem owners and availability implementations. Configuration selects one owner
for each index and one availability mode for the process. Peryx reads one [TOML](https://toml.io/) file passed with
`--config <path>`. Some operational settings also have flags or `PERYX_*` environment variables. Precedence is
`defaults < TOML file < environment < flags`.

## Top level

| Setting                   | Flag                | Environment                  | TOML key               | Default      |
| ------------------------- | ------------------- | ---------------------------- | ---------------------- | ------------ |
| Bind host                 | `--host`            | `PERYX_HOST`                 | `host`                 | `127.0.0.1`  |
| Bind port                 | `--port`            | `PERYX_PORT`                 | `port`                 | `4433`       |
| Data directory            | `--data-dir`        | `PERYX_DATA_DIR`             | `data_dir`             | `peryx-data` |
| Writer identity           | `--writer-identity` | `PERYX_WRITER_IDENTITY`      | `writer_identity`      | (none)       |
| Node identity             | `--node-identity`   | `PERYX_NODE_IDENTITY`        | `node_identity`        | (none)       |
| Offline mode              | `--offline`         | `PERYX_OFFLINE`              | `offline`              | `false`      |
| Read replica mode         | `--read-only`       | `PERYX_READ_ONLY`            | `read_only`            | `false`      |
| Config file               | `--config` / `-c`   | (n/a)                        | (n/a)                  | (none)       |
| Cache freshness (seconds) | (file/env only)     | `PERYX_CACHE_TTL_SECS`       | `cache_ttl_secs`       | `300`        |
| Page cache budget (bytes) | (file/env only)     | `PERYX_HOT_CACHE_BYTES`      | `hot_cache_bytes`      | `268435456`  |
| Upstream netrc file       | (file/env only)     | `PERYX_NETRC`                | `netrc`                | (none)       |
| Stale-on-error bound (s)  | (file/env only)     | `PERYX_MAX_STALE_SECS`       | `max_stale_secs`       | `300`        |
| Usage retention (days)    | (file/env only)     | `PERYX_USAGE_RETENTION_DAYS` | `usage_retention_days` | (unset)      |
| Indexes                   | (file only)         | (n/a)                        | `[[index]]`            | (see below)  |
| Rate limits               | (file only)         | (n/a)                        | `[rate_limit]`         | (see below)  |
| Background jobs           | (file only)         | (n/a)                        | `[jobs]`               | (see below)  |
| Availability mode         | (file only)         | (n/a)                        | `[availability]`       | `none`       |

Environment variables sit between the file and flags: a `PERYX_*` value overrides the TOML file, and a flag overrides
the variable. Only scalar settings are environment-configurable. The `[[index]]` topology, `[rate_limit]` block, and
`[availability]` table stay file-only, since none maps to a flat variable. An empty variable is treated as unset. The
`[log]` block also reads variables (`PERYX_LOG_LEVEL`, `PERYX_LOG_FORMAT`, `PERYX_LOG_SINK`, `PERYX_LOG_FILE`); see
[`[log]`](#log).

`host` accepts an IP literal or DNS hostname. Write IPv6 literals without URL brackets, such as `::` or `::1`; Peryx
adds brackets when it prints the listener URL. `peryx config check` rejects a host it cannot resolve and reports the
selected socket address. `serve` passes its selected address to the listener without parsing the host again.

`cache_ttl_secs` is both a fallback and a ceiling. When an upstream response carries a usable `Cache-Control` lifetime
(`s-maxage` or `max-age`) that is **shorter**, that lifetime governs the page; a longer one is clamped to
`cache_ttl_secs`. The fallback applies when the header is absent, `no-cache`/`no-store`, or zero.

The ceiling limits how long peryx trusts an upstream `Cache-Control` value. An upstream or CDN that answers
`max-age=31536000` would otherwise pin a page in the cache for a year with no revalidation. Raise `cache_ttl_secs` if
you want to trust a long upstream lifetime; lower it to revalidate sooner than the upstream asks. Set it to `0` to
revalidate every request. `cache_ttl_secs` must be non-negative.

Artifacts never expire; they are content-addressed by sha256, so a changed upstream file is a new entry on the page
rather than a mutation.

`max_stale_secs` bounds the other direction. When the upstream is unreachable or answers `5xx`, peryx keeps serving the
last page it fetched, but only for this long past the page's freshness window. Beyond it the upstream failure surfaces
instead, because a cache that answers with whatever it last saw, forever, has stopped being a cache and become a fork.
Set it to `0` to serve stale without limit, which is what mirroring a knowingly unreliable upstream asks for;
`offline = true` below is the unconditional form. `max_stale_secs` must be non-negative.

`usage_retention_days` bounds the durable
[daily group and source usage](@/core/operations/monitor.md#daily-group-and-source-usage) aggregate: buckets older than
this many days expire on the aggregator thread, off the request path. Leave it unset to keep every day. Tightening it
only reclaims durable storage; a retained day's totals never change, and expiry never blocks request handling.

`hot_cache_bytes` is the memory budget for transformed metadata pages. Each entry can be derived from the cached raw
page. A smaller budget lowers the hit rate, and `0` disables this cache. Ecosystem documentation states whether an
implementation uses the transformed-page cache.

`offline = true` disables upstream network access for cached indexes. Stored metadata and artifacts remain available; a
cold cached-index miss returns `503`. Virtual routes can still use a hosted layer. Run `peryx mirror sync` before
enabling offline mode on a machine that must work without network access.

`read_only = true` rejects each HTTP mutation with `503 Service Unavailable`, disables upstream cache fills, webhook
delivery, and background maintenance, and reports a read-only role through `GET /+status`. Under `none`, populate the
data directory through backup restore or external replication and omit `writer_identity`.

`writer_identity` enables the writer-claim guard for managed `dc` or `ha` replication. The configured value must match
the claim in the metadata store. Startup rejects `writer_identity` unless `[availability.replication]` supplies the
managed replication role. See [High availability](@/core/availability/high-availability.md) for promotion.

## Upstream credential sources

An upstream `password` or `token` reads from one of three places: the value inlined in the config, a `*_file` sibling,
or a `*_env` sibling. Set at most one per credential; naming two fails startup. The same three sources apply to every
`[[index.upstream]]` source. A bearer `token` takes precedence over `username` plus `password`, and both precede any
[netrc](#upstream-netrc-credentials) match, so adding a `_file` or `_env` source changes where the secret comes from,
never which credential wins.

```toml
[[index]]
name = "cache"
[[index.upstream]]
name = "primary"
url = "https://upstream.example/api/"
token_file = "/run/credentials/peryx.service/upstream-token"
credential_refresh_secs = 60
credential_refresh_on_unauthorized = true
credential_failure = "fail"
```

`password_file`/`token_file` fit secret files mounted read-only by the process manager: a
[systemd credential](https://systemd.io/CREDENTIALS/) under `$CREDENTIALS_DIRECTORY` (`LoadCredential=` or
`SetCredential=`), a [Kubernetes Secret](https://kubernetes.io/docs/concepts/configuration/secret/) projected under
`/run/secrets`, or a container-runtime secret. peryx trims surrounding whitespace, so a mounted file with a trailing
newline still resolves. `password_env`/`token_env` fit a value the manager passes as an environment variable, including
systemd's `%d`-free `Environment=`/`EnvironmentFile=` and a Kubernetes `secretKeyRef`.

By default, peryx resolves every source once when it builds the cached index and holds the value for the process
lifetime. Set `credential_refresh_secs` on an `[[index.upstream]]` source to reload a `_file` or `_env` credential
before a request after that interval has elapsed. Concurrent requests share one reload and read the current credential
without locking. Literal credentials and netrc entries cannot be refreshed.

`credential_refresh_on_unauthorized` defaults to `true`. When the upstream rejects a credential, peryx reloads its
source and replays the request once. The replay happens only when that credential generation has not already been
replaced, so a burst of rejected requests does not repeatedly read the source. `credential_failure = "fail"` rejects
requests until a later reload succeeds; `"anonymous"` retries without authentication. Credentials and derived bearer
tokens stay isolated by configured source and origin.

Projected Kubernetes Secret volumes and files replaced atomically by Vault Agent can rotate without restarting peryx.
Kubernetes `subPath` mounts do not receive projected Secret updates. Process managers ordinarily cannot change the
environment of an existing process, so `_env` refresh only observes changes made inside that process; restart peryx when
the manager changes an environment value. Refresh is lazy rather than a background task, and the interval is a minimum
between reads, not a promise that an idle source is read on schedule.

A missing, unreadable, empty, or oversized file and an unset, non-UTF-8, or empty environment variable each stop startup
or fail a refresh with an error that names only the file path or variable, never the secret. The one-mebibyte file
ceiling rejects a path pointed at a log or device before it is read into memory. Config snapshots (`peryx backup`) keep
the `_file`/`_env` reference and refresh policy, not the resolved secret.

To migrate an inlined credential, move the value into a file or environment variable and replace `password`/`token` with
its `_file`/`_env` sibling; the inline keys keep working, so migrate one upstream at a time.

### Exec credential helpers

Use `[index.upstream.credential_exec]` below a source when an identity system issues short-lived credentials on demand.
An exec helper replaces `username`, `password`, `token`, and their file/env forms. Its response expiry also replaces the
`credential_refresh_*` controls.

```toml
[[index]]
name = "cache"
[[index.upstream]]
name = "primary"
url = "https://upstream.example/api/"

[index.upstream.credential_exec]
argv = ["/usr/local/bin/peryx-credential", "--profile", "production"]
timeout_secs = 30
environment = ["UPSTREAM_TOKEN"]
failure = "fail"
```

peryx starts `argv` directly, without a shell, and writes one compact JSON object to standard input:

```json
{
  "version": 1,
  "origin": "https://upstream.example",
  "scope": "read"
}
```

The helper must write exactly one version-1 Basic or bearer response. `expires_at` is an RFC 3339 timestamp:

```json
{
  "version": 1,
  "expires_at": "2027-01-01T00:00:00Z",
  "type": "basic",
  "username": "service",
  "password": "secret"
}
```

```json
{
  "version": 1,
  "expires_at": "2027-01-01T00:00:00Z",
  "type": "bearer",
  "token": "secret"
}
```

The origin contains only scheme, host, and explicit port. Query strings, paths, and URL credentials never reach the
helper. `scope` is `read` for upstream metadata and artifacts. Unknown fields, response types, and protocol versions
fail closed.

The cached credential is shared by concurrent requests and stays on the lock-free provider path until 30 seconds before
expiry. A response already inside that safety margin fails instead of starting a helper on every request. One eligible
upstream `401` refreshes and replays a credential generation once; concurrent rejections share that refresh. A failed
helper is retried after 30 seconds. `failure = "fail"` rejects requests while no valid credential is available;
`"anonymous"` uses no authorization until a later refresh succeeds.

Execution is bounded to 64 argv items and 32 KiB of argv data, 1 to 300 seconds, 64 inherited environment names, 64 KiB
of standard output, and eight helpers across the process. The default timeout is 30 seconds. peryx clears the child
environment, restores only the named variables that exist, discards standard error, and kills and reaps the process
group on timeout or oversized output. Process arguments, output, upstream URL details, and returned credentials are not
included in diagnostics.

Do not put credentials in `argv`: process listings and config snapshots retain arguments. Pass only the environment
names a helper needs, keep the executable and its parent directories non-writable by the peryx service account, and
write no secret to standard error. The helper must treat standard input as untrusted and should return immediately after
writing its response.

A minimal helper can validate the request before returning a credential:

```python
#!/usr/local/bin/python3
import json
import os
import sys
from datetime import UTC, datetime, timedelta

request = json.load(sys.stdin)
if request != {
    "version": 1,
    "origin": "https://upstream.example",
    "scope": "read",
}:
    raise SystemExit(2)
json.dump(
    {
        "version": 1,
        "expires_at": (datetime.now(UTC) + timedelta(minutes=10)).isoformat(),
        "type": "bearer",
        "token": os.environ["UPSTREAM_TOKEN"],
    },
    sys.stdout,
    separators=(",", ":"),
)
```

## Upstream netrc credentials

Set `netrc` to opt into one shared file of Basic credentials for cached upstreams. peryx reads and parses the file once
at startup. A configured bearer token wins first, followed by a configured `username` and `password`; netrc supplies
credentials only when the `[[index.upstream]]` source has neither.

```toml
netrc = "/run/secrets/upstream.netrc"

[[index]]
name = "cache"
[[index.upstream]]
name = "primary"
url = "https://upstream.example/api/"
```

The host form matches an upstream on the scheme's default port:

```text
machine upstream.example
login service
password upstream-token
```

Use an origin or authority machine name when the same host serves more than one credential boundary. peryx searches an
exact origin first, then `host:port`, a bare host on a default port, and `default`.

```text
machine https://upstream.example:8443
login artifact-reader
password artifact-secret

machine upstream.example:9443
login staging-reader
password staging-secret
```

The resolved credential belongs to the configured upstream's exact scheme, host, and effective port. peryx does not send
it to an artifact URL on another origin, and reqwest removes it when a redirect changes any part of that origin. A
`default` entry can select credentials for an otherwise unmatched upstream, but those credentials remain bound to the
selected upstream after lookup; redirects do not trigger another netrc search.

The selected path must name a regular file. On Unix, its owner must match the effective process user and group or other
permission bits make startup fail; use `chmod 600 /run/secrets/upstream.netrc`. Parse and permission errors name the
file without printing its contents. Peryx does not search `~/.netrc` unless you select that path.

## Upstream TLS

A cached index can extend the platform trust store with a private CA and authenticate with a client certificate.
Configure the paths on each `[[index.upstream]]` source:

```toml
[[index]]
name = "cache"
[[index.upstream]]
name = "primary"
url = "https://upstream.example/api/"
ca_file = "/run/secrets/corp-ca.pem"
client_cert_file = "/run/secrets/peryx-client.pem"
client_key_file = "/run/secrets/peryx-client-key.pem"
```

Each source gets an independent trust store and identity. In an ordered route, put these keys on the source they apply
to; keys on the parent `[[index]]` are rejected because their scope would be ambiguous.

`ca_file` accepts one or more PEM certificates and adds them to the platform roots. It does not replace public trust.
`client_cert_file` contains a PEM leaf certificate followed by any intermediate certificates. `client_key_file` contains
its matching, unencrypted PEM private key. Configure the certificate and key together. Peryx parses the PEM and verifies
the key match when it builds the client. During the [TLS 1.3](https://www.rfc-editor.org/rfc/rfc8446) handshake, peryx
validates the upstream certificate and the upstream validates the client chain and
[RFC 5280](https://www.rfc-editor.org/rfc/rfc5280) purpose.

The identity is bound to the configured origin: scheme, host, and effective port. An explicit `artifact_url` or an
absolute artifact URL discovered in upstream metadata receives the configured CA but not the identity when it changes
origin. With a client identity, peryx rejects cross-origin redirects instead of offering the certificate to the redirect
target.

Peryx reads these files when it constructs the upstream client. To rotate them, replace each file atomically and restart
peryx through the deployment supervisor; peryx does not poll the files. Existing clients keep their parsed material
until the process replaces them. Restrict private keys to the peryx user (`0400` or `0600`) and their directory to
`0700`. In a container, mount the CA, certificate, and key as read-only secrets rather than copying them into the image.

Startup errors identify the index and whether the CA, certificate, or key was unreadable or invalid. They do not print
the path or PEM contents. Configuration snapshots retain the paths so a restore can remount the same secrets; they never
contain certificate or key bytes.

## TLS

peryx serves plain HTTP by default. Use it on loopback or behind a proxy that terminates TLS. To serve HTTPS from peryx,
configure one of two mutually exclusive tables. An unconfigured server keeps the plain HTTP path.

A `[tls]` table serves HTTPS from a certificate and key you provide:

```toml
[tls]
cert = "/etc/peryx/fullchain.pem"
key = "/etc/peryx/privkey.pem"
```

An `[acme]` table obtains and renews a certificate from an [ACME](https://datatracker.ietf.org/doc/html/rfc8555)
provider ([Let's Encrypt](https://letsencrypt.org/)), so a publicly reachable deployment serves trusted HTTPS with no
manual certificate handling and no client-side insecure flag:

```toml
[acme]
domains = ["artifacts.example.com"]
contact = "admin@example.com"
cache-dir = "/var/lib/peryx/acme"   # where issued certificates are cached; default "acme-cache"
staging = false                     # true uses Let's Encrypt staging while testing
```

| Table    | Key         | Meaning                                                     | Default      |
| -------- | ----------- | ----------------------------------------------------------- | ------------ |
| `[tls]`  | `cert`      | PEM certificate chain                                       | (required)   |
| `[tls]`  | `key`       | PEM private key                                             | (required)   |
| `[acme]` | `domains`   | Domains to request a certificate for; reachable on port 443 | (required)   |
| `[acme]` | `contact`   | Contact email the ACME account registers                    | (required)   |
| `[acme]` | `cache-dir` | Where certificates and the account key are cached           | `acme-cache` |
| `[acme]` | `staging`   | Use the provider's staging environment                      | `false`      |

For an `[acme]` deployment the domain's DNS must point at the server and port 443 must be reachable, since the ACME
handshake happens there. Behind a load balancer or reverse proxy that already terminates TLS, leave both tables unset
and let the proxy hold the certificate.

## `[[index]]`

Each `[[index]]` table declares one index. `name` is required; exactly one of `[[index.upstream]]`, `hosted`, or
`layers` selects the role. peryx rejects unknown keys.

| Key                    | Role    | Meaning                                                              | Default                             |
| ---------------------- | ------- | -------------------------------------------------------------------- | ----------------------------------- |
| `name`                 | all     | Identifier other indexes reference in `layers`                       | (required)                          |
| `route`                | all     | URL prefix the index is served under                                 | same as `name`                      |
| `ecosystem`            | all     | Owner registration ID                                                | unique lowest-priority registration |
| `upstream`             | cached  | Ordered `[[index.upstream]]` sources to cache (see below)            |                                     |
| `fallback`             | cached  | Continue to the next source when one has no match                    | `true`                              |
| `protected`            | cached  | Resource globs a later source may not shadow                         | none                                |
| `pins`                 | cached  | Map of resource to the source name that serves it                    | none                                |
| `upstream_concurrency` | cached  | Cap on concurrent upstream fetches; `0` is unlimited and the default | `0`                                 |
| `offline`              | cached  | Serve this cached index from disk only                               | `false`                             |
| `prefetch`             | cached  | Resource and artifact selection for `peryx mirror`                   | (see below)                         |
| `hosted`               | hosted  | `true` marks this index as a hosted store                            | `false`                             |
| `volatile`             | hosted  | Allow delete and overwrite                                           | `true`                              |
| `anonymous_read`       | all     | Whether a credential-less request may read this index                | `[auth]` default                    |
| `access_token`         | all     | Named credentials the index accepts, with scoped grants              | none                                |
| `layers`               | virtual | Ordered index names to compose                                       |                                     |
| `write_target`         | virtual | Hosted layer that receives writes                                    | first hosted layer                  |
| `policy`               | all     | Nested index policy table                                            | empty                               |
| `settings`             | all     | Nested table of the index ecosystem's own settings                   | empty                               |
| `webhook`              | all     | Signed delivery targets for owner-defined events                     | none                                |

A hosted index accepts writes through its `[[index.access_token]]` grants; there is no separate write credential key.

A `route` is a raw URL path prefix. It must be one or more non-empty path segments separated by `/`; each segment may
contain only ASCII letters, digits, `-`, `.`, `_`, and `~`. Startup rejects routes with a leading or trailing `/`, empty
segments, percent encoding, traversal segments, control characters, spaces, and routes whose first segment is reserved
for a Peryx endpoint: `+stats`, `+status`, `_`, `admin`, `api-docs`, `browse`, `favicon.svg`, `metrics`, `pkg`,
`search`, `stats`, or `upload`. These are the paths peryx's own API and web UI serve. The `_` segment reserves
authentication routes, so an index may not shadow one.

Declaring any `[[index]]` replaces the default topology. Supported implementations document their defaults and complete
examples:

- [Ecosystem owner documentation](@/ecosystems/_index.md)

When `ecosystem` is omitted, Peryx selects the unique lowest-priority registration. It rejects duplicate priorities
during registry construction. An explicit ID must name a shipped owner; `config check` and `serve` reject an unknown ID
before opening storage or binding a socket. Unselected owners remain linked but register no capability, run no
installer, mount no route, and start no work.

Startup rejects duplicate names, duplicate routes, invalid routes, `layers` entries that name no index, `layers` that
mix ecosystems, a `write_target` that is not a hosted index, and an empty `[[index.access_token]]` `secret`. An empty
secret is a configuration error rather than a valid value: authorization compares a token secret against the Basic-auth
password on each request, so a blank string would admit any request that presents an empty password. The empty value
almost always comes from a config template whose environment variable never expanded, so peryx refuses it at load time
instead of booting into that state. To grant no write access, configure no write-granting token.

### `[[index.upstream]]`

A cached index caches one or more upstream sources, each declared by an `[[index.upstream]]` table under it. One source
is the common case; several make an ordered route where the first source with a match answers and `fallback` controls
whether a miss continues to the next. Each source carries its own URL, credentials, and TLS, so the credential and TLS
keys below belong on the source, never on the parent `[[index]]`.

| Key                                  | Meaning                                                          | Default    |
| ------------------------------------ | ---------------------------------------------------------------- | ---------- |
| `name`                               | Identifier for the source, unique within the index               | (required) |
| `url`                                | Upstream API URL                                                 | (required) |
| `artifact_url`                       | Origin that serves artifacts when it differs from `url`          | `url`      |
| `username`                           | Basic-auth username for the upstream                             | (none)     |
| `password`                           | Basic-auth password; `password_file`/`password_env` read it out  | (none)     |
| `token`                              | Bearer token; takes precedence over username/password            | (none)     |
| `token_file`                         | Path to read `token` from instead of inlining it                 | (none)     |
| `token_env`                          | Environment variable to read `token` from instead of inlining it | (none)     |
| `credential_exec`                    | Nested short-lived credential helper configuration               | (none)     |
| `credential_refresh_secs`            | Minimum seconds between credential source reads                  | (none)     |
| `credential_refresh_on_unauthorized` | Reload and replay once after credential rejection                | `true`     |
| `credential_failure`                 | Reload failure behavior: `fail` or `anonymous`                   | `fail`     |
| `ca_file`                            | PEM CA bundle added to platform trust for this source            | (none)     |
| `client_cert_file`                   | PEM client certificate chain; requires `client_key_file`         | (none)     |
| `client_key_file`                    | Matching unencrypted PEM client key; requires `client_cert_file` | (none)     |

```toml
[[index]]
name = "combined"
fallback = true
protected = ["internal-resource"]

[index.pins]
internal-resource = "internal"

[[index.upstream]]
name = "internal"
url = "https://internal.example/api/"
username = "reader"
password_file = "/run/secrets/internal-password"

[[index.upstream]]
name = "public"
url = "https://public.example/api/"
```

### `[index.policy]`

Policy has a common core and optional implementation rules. Core owns name allow and block lists, protected names, size
limits, repository quotas, and audit mode. The selected owner compiles owner-specific keys. Unknown or inapplicable keys
fail startup.

| Common key                | Meaning                                                             |
| ------------------------- | ------------------------------------------------------------------- |
| `allow_resources`         | Only these normalized resource identities may be served or mirrored |
| `block_resources`         | Denied normalized resource identities                               |
| `protected_resources`     | Exact resources or `prefix-*` namespaces that cannot fall back      |
| `max_artifact_size_bytes` | Largest artifact accepted or served                                 |
| `max_resource_size_bytes` | Largest logical artifact total for one resource                     |
| `max_accounted_bytes`     | Deduplicated bytes charged to one repository                        |
| `max_resources`           | Distinct resources charged to one repository                        |
| `quota_audit`             | Record a would-reject quota decision and admit the write            |

Owner documentation lists supported policy keys and their behavior:

- [Ecosystem owner documentation](@/ecosystems/_index.md)

### `[index.settings]`

Core keeps this table opaque and passes it to the selected owner. That owner owns its keys, defaults, validation, and
compiled representation. Unsupported keys fail startup.

- [Ecosystem owner documentation](@/ecosystems/_index.md)

### `[[index.access_token]]`

Each `[[index.access_token]]` table adds one named credential the index accepts, with a grant scoped to some resources
and actions. Put these under the hosted index that stores the writes. The
[access model](@/core/access/authentication.md) covers the grammar; the keys are:

```toml
[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
resources = ["team-*"]
actions = ["write", "delete"]
expires_at = "2027-01-01T00:00:00Z"
```

| Key           | Meaning                                                                      | Default    |
| ------------- | ---------------------------------------------------------------------------- | ---------- |
| `name`        | Subject the token authenticates as; unique per index                         | (required) |
| `secret`      | Shared secret the ecosystem authentication adapter verifies                  | (required) |
| `secret_file` | Path to read `secret` from instead of inlining it                            | (none)     |
| `resources`   | Resource globs the token may act on; `*` matches any run of characters       | `["*"]`    |
| `actions`     | Any of `read`, `write`, `delete`; at least one                               | (required) |
| `expires_at`  | [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) time after which it stops | never      |

A token needs exactly one of `secret` and `secret_file`. The selected owner maps client credentials and routes onto
shared actions. Supported access implementations:

- [Ecosystem owner documentation](@/ecosystems/_index.md)

### `[index.prefetch]`

Cached indexes can declare defaults for `peryx mirror plan`, `peryx mirror sync`, and `peryx mirror verify`. Core keeps
this table opaque and passes it to the selected owner. That owner owns its keys, defaults, validation, and CLI override
rules. An unsupported key fails when the mirror command compiles the selection.

- [Ecosystem owner documentation](@/ecosystems/_index.md)

## `[rate_limit]`

Rate limits are local to one peryx process and disabled by default. When `enabled = true`, they use fixed windows and
bounded in-memory buckets; restarting the process clears the buckets. `max_clients` caps the number of client/class
buckets kept in memory. Set a class `requests` or `window_secs` to `0` to disable that class limit.

peryx hashes a named principal with a process-random seed and stores neither the credential nor the principal name.
Invalid credentials and routes without an authentication driver use the socket peer IP. Without socket metadata, peryx
uses `127.0.0.1` and ignores forwarding headers.

Set `trusted_proxies` to the reverse-proxy networks from which peryx accepts forwarding headers. For a matching socket
peer, peryx scans `X-Forwarded-For` from the nearest hop and selects the first address outside the trusted networks. If
the header is absent, peryx accepts one `X-Real-IP` value. The same peer check gates `X-Forwarded-Host` and
`X-Forwarded-Proto` for public links, whether or not `enabled` is true. Peryx uses the socket peer when the trusted
client-address suffix is malformed or the chain contains trusted addresses throughout. It treats IPv4-mapped IPv6
addresses as their IPv4 equivalents. Leave the list empty for direct deployments. Exclude client networks and
intermediaries that accept caller-supplied forwarding headers.

Clients can change a Basic username or bearer value without changing buckets when both values resolve to the same
principal. peryx groups rotated invalid `Authorization` values under the peer IP.

| Setting           | Meaning                                                  | Default |
| ----------------- | -------------------------------------------------------- | ------- |
| `enabled`         | Install the HTTP request limiter                         | `false` |
| `max_clients`     | Maximum client/class buckets kept in memory              | `8192`  |
| `trusted_proxies` | IPv4 and IPv6 networks allowed to set forwarding headers | `[]`    |

Each route class is a sub-table with `requests` and `window_secs`:

| Table                   | Route class                                     | Default        |
| ----------------------- | ----------------------------------------------- | -------------- |
| `[rate_limit.listing]`  | Resource listings and detail pages              | `600` / `60s`  |
| `[rate_limit.metadata]` | Separate metadata objects                       | `1200` / `60s` |
| `[rate_limit.artifact]` | Artifact reads and inspection                   | `300` / `60s`  |
| `[rate_limit.upload]`   | Mutation requests                               | `60` / `60s`   |
| `[rate_limit.admin]`    | Status, stats, metrics, and discovery endpoints | `120` / `60s`  |

The selected ecosystem maps each route to one request group. Read methods retain the group of the resource they address;
the method alone does not turn a read into a write. Ecosystem documentation defines each route map:

- [Ecosystem owner documentation](@/ecosystems/_index.md)

Example:

```toml
[rate_limit]
enabled = true
max_clients = 4096
trusted_proxies = ["127.0.0.1/32", "10.42.0.0/16"]

[rate_limit.listing]
requests = 300
window_secs = 60

[[index]]
name = "cache"
upstream_concurrency = 4
[[index.upstream]]
name = "primary"
url = "https://upstream.example/api/"
```

## `[jobs]`

The `[jobs]` table controls the node-local background work peryx runs on a timer: reclaiming expired process resources
and revalidating stale cached pages. Each ecosystem is swept on its own, so independent repositories run together while
one repository never sweeps itself twice at once.

```toml
[jobs]
mode = "none"
```

| Key    | Meaning                                                    | Default |
| ------ | ---------------------------------------------------------- | ------- |
| `mode` | `local` runs maintenance on this node; `none` runs nothing | `local` |

`mode = "none"` starts no scheduler, timer, or worker, which suits a node fronted by an external maintenance runner or
one that should only serve. A [read replica](@/core/availability/high-availability.md) runs no maintenance regardless of
this setting.

### Schedules

By default a node runs cache maintenance once a minute. Replace that with an explicit `[[jobs.schedule]]` array to
choose which jobs run and how often. Each entry names one registered job and a positive interval in seconds; peryx
rejects a non-positive interval at startup, naming the schedule's index (`jobs schedule [0]`).

```toml
[[jobs.schedule]]
job = "cache_maintenance"
interval_secs = 300
```

| Key             | Meaning                                        | Default    |
| --------------- | ---------------------------------------------- | ---------- |
| `job`           | Registered core or ecosystem job               | (required) |
| `interval_secs` | Seconds between runs, must be positive         | (required) |
| `concurrency`   | Copies in flight for `dc_copy`; range `1..=64` | `8`        |

`cache_maintenance` reclaims expired process resources and revalidates stale cached pages, with one run per active
owner. Independent repositories can sweep together, while one repository cannot run two sweeps at once.

`dc_copy` copies blobs owed by the local placement domain from verified peers. It runs on a filesystem backend in a
configured `dc` or `ha` group whose roster names this node and at least one peer. It accepts only `concurrency` (copies
in flight, range `1..=64`, default `8`). The ownership group's cluster term fences each copy, so a node without a live
consensus term copies nothing.

One bounded timer drives every schedule, so a large set costs no per-tick scan. When a tick arrives while the same job's
previous run is still going, peryx skips it rather than queueing it, and counts the skip in the job metrics. Pick an
interval longer than a sweep takes, and stagger it clear of your peak request hours so maintenance and traffic do not
contend for upstream bandwidth.

The timer keeps no durable state. On restart it sets each schedule's next run one full interval after startup and drops
the occurrences missed while the process was down rather than replaying them as a backlog.

Ecosystem documentation lists registered jobs and their schedule keys. `peryx job list` and `peryx job show <id>`
inspect durable run history.

## `[blob]`

The `[blob]` table selects where blobs live: the local filesystem (the default) or an S3-compatible object store.
Metadata always stays in the local redb store; only the content-addressed blobs move. Omit the table to keep blobs under
`data_dir/blobs`.

```toml
[blob]
backend = "s3"
endpoint = "https://s3.us-east-1.amazonaws.com"
bucket = "peryx-blobs"
region = "us-east-1"
prefix = "prod"
path_style = false
timeout_secs = 30
max_retries = 3
multipart_threshold_bytes = 16777216
part_size_bytes = 16777216
upload_concurrency = 4
conditional_writes = true
checksum_writes = true
```

| Key                         | Meaning                                                           | Default      |
| --------------------------- | ----------------------------------------------------------------- | ------------ |
| `backend`                   | `filesystem` or `s3`                                              | `filesystem` |
| `endpoint`                  | Base URL of the S3-compatible service (http or https)             | (required)   |
| `bucket`                    | Bucket that holds the blobs                                       | (required)   |
| `region`                    | Signing region                                                    | (required)   |
| `prefix`                    | Key prefix inside the bucket; blobs land at `<prefix>/sha256/...` | (none)       |
| `path_style`                | `true` for path-style addressing (MinIO); `false` virtual-hosted  | `false`      |
| `timeout_secs`              | Per-request timeout, in seconds                                   | `30`         |
| `max_retries`               | Retries for a transient transport or 5xx/429 response             | `3`          |
| `multipart_threshold_bytes` | Objects at or below this size upload in one `PUT`                 | `16777216`   |
| `part_size_bytes`           | Multipart part size, from 5 MiB through 5 GiB                     | `16777216`   |
| `upload_concurrency`        | Parts uploaded at once during a multipart upload                  | `4`          |
| `conditional_writes`        | Endpoint enforces `If-None-Match` create-if-absent writes         | `true`       |
| `checksum_writes`           | Endpoint validates the SHA-256 checksum sent with each write      | `true`       |

Endpoint base paths are preserved. User information, queries, and fragments are rejected because credentials belong in
the AWS provider chain rather than the endpoint URL.

Blobs are immutable and keyed by their sha256. A write stages to `data_dir/blob-staging`, hashes as it streams, then
uses a conditional create for `<prefix>/sha256/<digest>`: one `PUT` below `multipart_threshold_bytes`, bounded
concurrent parts above it. Peryx journals multipart upload IDs under the staging directory so a commit interrupted after
creation can resume. Reads stream ranged `GET`s. An explicit blob verification downloads and hashes the complete object;
normal reads do not add a second digest pass.

### Credentials

The `[blob]` table does not hold secrets. The first S3 request resolves credentials through the AWS SDK default provider
chain: environment variables, shared config and credentials files, web identity, ECS task credentials, or EC2 instance
metadata. These providers cache and refresh temporary credentials. The bucket policy must allow `s3:GetObject`,
`s3:PutObject`, `s3:DeleteObject`, and `s3:AbortMultipartUpload` on `<prefix>/*`, plus `s3:GetBucketLocation` on the
bucket for health checks.

### Durability capabilities

Each backend proves a durability scope and the atomic-write evidence a completed write carries, resolved once at
startup. The filesystem backend commits within a single host's failure domain behind an atomic rename that refuses to
clobber an existing blob and publishes only bytes that hash to the expected digest. An S3-compatible backend commits
within its object store's failure domain, and what it can prove there depends on the endpoint, not the provider brand:
AWS S3 honors `If-None-Match` create-if-absent writes and validates the SHA-256 checksum on every write, while some
S3-compatible gateways reject the `*` precondition or the checksum header. The operator declares each guarantee per
instance with `conditional_writes` and `checksum_writes`, both `true` by default; set the one your endpoint lacks to
`false`, and peryx stops sending that header so writes keep succeeding.

`[availability]` modes that replicate their acknowledgement read these capabilities before serving traffic, because a
mode that acknowledges a write across nodes cannot treat a bare storage success as proof. `none` acknowledges from local
durability alone and accepts any backend. `dc` and `ha` require both conditional create-if-absent and checksum-validated
writes, so startup rejects a `dc` or `ha` mode backed by an S3 endpoint declared without one of them, naming the missing
guarantee and never the endpoint, bucket, or credentials. The filesystem backend proves both, so it satisfies every
mode.

### Backup and failure recovery

Because the object write commits the blob before its metadata row, a crash between the two leaves an orphan object. A
later write of the same content observes the existing digest key. Configure bucket-level versioning, replication, or
backups for the object bytes, and keep an independent recovery point for Peryx's local metadata store.
`peryx backup create` rejects an S3-backed configuration before writing a backup; it does not snapshot the `[blob]`
selection or the metadata that refers to those objects.

## `[availability]`

The shipped binary contains all availability implementations. Only `[availability].mode` selects one; command-line flags
and alternate builds do not override it. `mode` accepts `none`, `dc`, or `ha`, with acknowledgement guarantees defined
by the [availability contracts](@/core/availability/contracts.md). An omitted table and `mode = "none"` resolve to no
managed availability resources.

```toml
[availability]
mode = "none"
```

`none` opens no replication client, availability route, listener, metric family, or task. It can serve writes or run
with `read_only = true`. `dc` and `ha` select distributed coordination; each needs a `[availability.replication]` role.
Peryx rejects an unknown mode during config resolution. It also rejects a distributed mode without that role or a
replication role under `none`. The process exits before opening storage or binding a listener; it starts no worker.

```toml
[availability]
mode = "dc"

[availability.replication]
role = "primary"
source = "writer-a"
token_file = "/run/secrets/replication-token"
```

| Key    | Meaning               | Default |
| ------ | --------------------- | ------- |
| `mode` | `none`, `dc`, or `ha` | `none`  |

The nested `[availability.replication]` table declares this node's replication role. `role = "primary"` serves the
replication journal other nodes copy; `role = "replica"` follows a primary and, like `read_only`, refuses client
mutations. peryx rejects an unknown key in either table, naming the offending field.

| Key                  | Role    | Meaning                                                         | Default    |
| -------------------- | ------- | --------------------------------------------------------------- | ---------- |
| `role`               | both    | `primary` or `replica`                                          | (required) |
| `source`             | primary | This writer's stable name in the replication journal            | (required) |
| `upstream`           | replica | URL of the primary this replica follows                         | (required) |
| `token`              | both    | Shared replication credential, inline                           | (none)     |
| `token_file`         | both    | Path to read `token` from instead of inlining it                | (none)     |
| `poll_interval_secs` | replica | Seconds between change-journal polls, must be positive          | `1`        |
| `page_size`          | replica | Changes fetched per poll, positive and within the primary limit | `100`      |

A role needs exactly one of `token` or `token_file`; setting both, or neither, is rejected. Keep the credential out of
the config file with `token_file`, the path to a mounted orchestrator secret or a systemd credential, which peryx reads
at startup and never logs. A configuration snapshot (`peryx backup`) preserves a `token_file` as its path and never
resolves the secret behind it into the manifest.

A replica commits a page's metadata as soon as it arrives and pulls the whole blobs the page references on an
independent frontier, so a slow blob transfer never holds up the metadata behind it. A read waits on the slower of the
metadata and blob frontiers, so a record never appears before the bytes it names.

### Static datacenter membership

A `dc` or `ha` node may also declare the static replication group it belongs to. The group names one writer and its read
replicas explicitly; peryx never infers a member from a network broadcast, and no liveness timeout ever promotes a
replica. Losing the writer stops new writes until the configured writer returns or an operator performs the later fenced
transfer procedure, so a replacement is an explicit configuration edit, reviewed like any other, rather than an
automatic election.

```toml
[availability]
mode = "dc"
group = "east"

[availability.replication]
role = "primary"
source = "writer-a"
token_file = "/run/secrets/replication-token"

[[availability.member]]
node = "writer-a"
dc = "dc-east"
address = "https://a.internal:8443"
role = "writer"

[[availability.member]]
node = "replica-b"
dc = "dc-east"
address = "https://b.internal:8443"
role = "replica"
```

The roster is validated alongside the `[availability.replication]` role. Each `[[availability.member]]` table declares
one member.

| Key       | Meaning                                                            | Default                  |
| --------- | ------------------------------------------------------------------ | ------------------------ |
| `group`   | The group identity the roster belongs to                           | (required with a roster) |
| `node`    | The member's stable identity, unique within the group              | (required)               |
| `dc`      | Failure-domain label; unique per member in `ha`, shareable in `dc` | (required)               |
| `address` | The address peers reach the member on, unique within the group     | (required)               |
| `role`    | `writer` or `replica`                                              | (required)               |

In `ha` mode each node also sets `node_identity` to its own member's `node` value, so the ownership consensus runs under
that node's own voter identity. This is distinct from `writer_identity`, which names the one writer every node claims
and follows on the metadata plane and is therefore identical across the group: deriving the consensus identity from
`writer_identity` would make every node share the writer's voter, so no multi-voter group would form and a home failure
could not transfer authority to a survivor.

peryx validates the group at startup and refuses to serve on any violation: a blank or duplicated `group`, `node`, or
`address`; a `node` that reuses the `group` identity; anything other than exactly one `writer`; or a group with no
configured replica. Under `ha`, each member needs a distinct `dc` because the datacenter identifies its consensus voter;
`dc` mode permits several members in one datacenter. peryx never probes a member's `address`, so an unreachable
configured peer is a valid topology, not a configuration error. A roster requires `dc` or `ha` mode; declaring one under
`none` is rejected, naming the `availability` field.

## `[auth]`

The `[auth]` table holds the access settings every index shares: the signing key of peryx's token realm, the lifetime of
a minted token, and the default each index's `anonymous_read` takes. All keys are optional.

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
token_ttl_secs = 300
default_anonymous_read = false
```

| Key                      | Meaning                                                              | Default |
| ------------------------ | -------------------------------------------------------------------- | ------- |
| `signing_key`            | Secret peryx signs its own tokens with                               | (none)  |
| `signing_key_file`       | Path to read `signing_key` from instead of inlining it               | (none)  |
| `token_ttl_secs`         | Lifetime from 1 through 86400 seconds; OCI token realm minimum is 60 | `300`   |
| `default_anonymous_read` | What an index's `anonymous_read` defaults to when the index omits it | `true`  |

Set at most one of `signing_key` and `signing_key_file`. peryx reads the key at startup. Implementations that mint
tokens cap their lifetime at `token_ttl_secs`. Peryx requires 60 through 86400 when a signing key and OCI index enable
the OCI token realm. A deployment with no OCI index accepts 1 through 86400. `default_anonymous_read = false` sets the
anonymous-read default once instead of adding a flag to each index. See
[signing key](@/core/access/authentication.md#signing-key) for the 32-byte minimum, generation, and coordinated
rotation.

Each `[[auth.ldap_provider]]` configures one named StartTLS directory and optional exact group-to-role mappings. It
supports direct user DNs and service-account search followed by a user bind. Provider URLs, attributes, trust files,
password sources, timeouts, and the total connection bound are listed under
[LDAP providers](@/core/access/authentication.md#ldap-providers). Configuring a provider constructs the login service
but does not add an HTTP login route or browser session.

## `[[index.webhook]]`

Put webhook tables under the index that should emit them. A target on a virtual index receives events for requests made
through that route. The [ecosystem owner documentation](@/ecosystems/_index.md) defines event names and payload schemas.

```toml
[[index]]
name = "combined"
layers = ["hosted", "cache"]
write_target = "hosted"

[[index.webhook]]
name = "ci"
url = "https://ci.example/hooks/peryx"
secret_env = "PERYX_WEBHOOK_SECRET"
```

| Key          | Meaning                                                 | Default    |
| ------------ | ------------------------------------------------------- | ---------- |
| `name`       | Stable target name used in delivery logs                |            |
| `url`        | HTTP or HTTPS endpoint                                  | (required) |
| `secret`     | Literal HMAC signing secret                             | (none)     |
| `secret_env` | Environment variable containing the HMAC signing secret | (none)     |
| `events`     | Event names to send; empty selects all supported events | all        |

Use one of `secret` or `secret_env`. The resolved value must meet the
[webhook secret requirements](@/core/operations/webhooks.md#secret-strength). Event names come from the selected owner.
An empty `events` list subscribes to each event that implementation emits.

Peryx stores pending deliveries in the metadata database and sends them outside the request path. Transport failures and
HTTP `5xx` responses retry up to five attempts with capped backoff of 5, 15, 45, then 135 seconds; `408 Request Timeout`
and `429 Too Many Requests` use the same retry path. Other `4xx` responses fail after one attempt.

Redirects also fail after one attempt. Peryx neither follows nor retries a `3xx` response because sending the signed
payload to a target-selected location could move it outside the configured origin. A `302` stores
`webhook target returned redirect 302; redirects are not followed` as its last error.

The delivery log stores the payload, target name, attempt count, next retry time, response status, and last error. It
does not store webhook secrets.

## `[log]`

| Key      | Values                                                                                                           | Default  |
| -------- | ---------------------------------------------------------------------------------------------------------------- | -------- |
| `level`  | [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) | `info`   |
| `format` | `pretty`, `json`                                                                                                 | `pretty` |
| `sink`   | `stdout`, `file`, `journald`, `syslog`                                                                           | `stdout` |
| `file`   | Path required when `sink = "file"`                                                                               | None     |

The flags `--log-level`, `--log-format`, `--log-sink`, `--log-file`, `-v`, and `-vv` override these, as do the
`PERYX_LOG_LEVEL`, `PERYX_LOG_FORMAT`, `PERYX_LOG_SINK`, and `PERYX_LOG_FILE` variables (below the flags in precedence).
