+++
title = "Configuration"
description = "Every TOML key, flag, and default. Precedence is defaults < TOML file < environment < flags."
weight = 1
+++

peryx reads one [TOML](https://toml.io/) file, passed with `--config <path>`. A few operational settings double as flags
or `PERYX_*` environment variables, which override the file. Precedence is `defaults < TOML file < environment < flags`.

## Top level

| Setting | Flag | Environment | TOML key | Default | | ------------------------- | ------------------- |
---------------------------- | ---------------------- | ------------ | | Bind host | `--host` | `PERYX_HOST` | `host` |
`127.0.0.1` | | Bind port | `--port` | `PERYX_PORT` | `port` | `4433` | | Data directory | `--data-dir` |
`PERYX_DATA_DIR` | `data_dir` | `peryx-data` | | Writer identity | `--writer-identity` | `PERYX_WRITER_IDENTITY` |
`writer_identity` | (none) | | Node identity | `--node-identity` | `PERYX_NODE_IDENTITY` | `node_identity` | (none) | |
Offline mode | `--offline` | `PERYX_OFFLINE` | `offline` | `false` | | Read replica mode | `--read-only` |
`PERYX_READ_ONLY` | `read_only` | `false` | | Config file | `--config` / `-c` | (n/a) | (n/a) | (none) | | Cache
freshness (seconds) | (file/env only) | `PERYX_CACHE_TTL_SECS` | `cache_ttl_secs` | `300` | | Page cache budget (bytes)
| (file/env only) | `PERYX_HOT_CACHE_BYTES` | `hot_cache_bytes` | `268435456` | | Upstream netrc file | (file/env only)
| `PERYX_NETRC` | `netrc` | (none) | | Stale-on-error bound (s) | (file/env only) | `PERYX_MAX_STALE_SECS` |
`max_stale_secs` | `300` | | Usage retention (days) | (file/env only) | `PERYX_USAGE_RETENTION_DAYS` |
`usage_retention_days` | (unset) | | Indexes | (file only) | (n/a) | `[[index]]` | (see below) | | Rate limits | (file
only) | (n/a) | `[rate_limit]` | (see below) | | Background jobs | (file only) | (n/a) | `[jobs]` | (see below) | |
Availability mode | (file only) | (n/a) | `[availability]` | `none` |

Environment variables sit between the file and flags: a `PERYX_*` value overrides the TOML file, and a flag overrides
the variable. Only scalar settings are environment-configurable. The `[[index]]` topology, `[rate_limit]` block, and
`[availability]` table stay file-only, since none maps to a flat variable. An empty variable is treated as unset. The
`[log]` block also reads variables (`PERYX_LOG_LEVEL`, `PERYX_LOG_FORMAT`, `PERYX_LOG_SINK`, `PERYX_LOG_FILE`); see
[`[log]`](#log).

`cache_ttl_secs` is both a fallback and a ceiling. When an upstream response carries a usable `Cache-Control` lifetime
(`s-maxage` or `max-age`) that is **shorter**, that lifetime governs the page; a longer one is clamped to
`cache_ttl_secs`. The fallback applies when the header is absent, `no-cache`/`no-store`, or zero.

The ceiling matters because `Cache-Control` is the upstream's opinion, not yours. An upstream — or any CDN in front of
it — answering `max-age=31536000` would otherwise pin a page in your cache for a year with no revalidation. Raise
`cache_ttl_secs` if you want to trust a long upstream lifetime; lower it to revalidate sooner than the upstream asks.

Artifacts never expire; they are content-addressed by sha256, so a changed upstream file is a new entry on the page
rather than a mutation.

`max_stale_secs` bounds the other direction. When the upstream is unreachable or answers `5xx`, peryx keeps serving the
last page it fetched rather than failing a build over a blip — but only for this long past the page's freshness window.
Beyond it the upstream failure surfaces instead, because a cache that answers with whatever it last saw, forever, has
stopped being a cache and become a fork. Set it to `0` to serve stale without limit, which is what mirroring a knowingly
unreliable upstream asks for; `offline = true` below is the unconditional form.

`usage_retention_days` bounds the durable
[daily version and source usage](@/core/monitor.md#daily-version-and-source-usage) aggregate: buckets older than this
many days expire on the aggregator thread, off the request path. Leave it unset to keep every day. Tightening it only
reclaims durable storage; a retained day's totals never change, and expiry never blocks request handling.

`hot_cache_bytes` is the memory budget for the transformed-page cache, where a warm request is a lookup, an expiry
check, and a memcpy. It trades memory against warm-serve speed and nothing else: every entry is re-derivable from the
cached raw page, so a smaller budget only lowers the hit rate, and `0` turns the cache off so each warm page pays its
transform again. Lower it on a memory-tight host; raise it when a few projects with very large index pages (`boto3` and
`numpy` run to megabytes of JSON) carry the traffic. The PyPI driver is the only ecosystem that populates it today.

`offline = true` disables upstream network access for configured cached indexes. Whatever an ecosystem has cached serves
from disk: PyPI project pages, [PEP 658](https://peps.python.org/pep-0658/) metadata siblings, and wheels; OCI manifests
and blobs. A cold cached-index miss returns `503`; virtual-index routes still serve any hosted layer that can answer.
Use `peryx mirror sync` before enabling offline mode on a machine that must run without network access.

`read_only = true` runs the process as a [read replica](@/core/high-availability.md). It rejects each HTTP mutation with
`503 Service Unavailable`, disables upstream cache fills, webhook delivery, and background maintenance, and reports the
replica role through `GET /+status`. Use this mode only with a data directory populated by backup restore or an external
replication system. Replica mode requires the copied metadata store and configuration to contain the same nonblank
`writer_identity`; peryx stops startup unless both values match.

`writer_identity` enables the single-writer startup guard. A writer claims this value in the metadata store at startup;
another configured identity cannot start against that store. Replica mode does not claim it, so a restored config
snapshot may retain the writer's value while serving read-only. See [High availability](@/core/high-availability.md) for
promotion.

## Upstream credential sources

An upstream `password` or `token` reads from one of three places: the value inlined in the config, a `*_file` sibling,
or a `*_env` sibling. Set at most one per credential; naming two fails startup. The same three sources apply to every
`[[index.upstream]]` source, and to both PyPI and OCI upstreams. A bearer `token` takes precedence over `username` plus
`password`, and both precede any [netrc](#upstream-netrc-credentials) match, so adding a `_file` or `_env` source
changes where the secret comes from, never which credential wins.

```toml
[[index]]
name = "corp"
[[index.upstream]]
name = "primary"
url = "https://packages.corp.example/simple/"
username = "peryx"
password_env = "PERYX_CORP_PASSWORD"          # from the environment the process manager injects

[[index]]
name = "registry"
ecosystem = "oci"
[[index.upstream]]
name = "primary"
url = "https://registry.corp.example"
token_file = "/run/credentials/peryx.service/registry-token" # from a systemd credential
credential_refresh_secs = 60
credential_refresh_on_unauthorized = true
credential_failure = "fail"
```

`password_file`/`token_file` fit secret files mounted read-only by the process manager: a
[systemd credential](https://systemd.io/CREDENTIALS/) under `$CREDENTIALS_DIRECTORY` (`LoadCredential=` or
`SetCredential=`), a [Kubernetes Secret](https://kubernetes.io/docs/concepts/configuration/secret/) projected under
`/run/secrets`, or a Docker secret. peryx trims surrounding whitespace, so a file that a `kubectl create secret` mount
or an `echo` left with a trailing newline still resolves. `password_env`/`token_env` fit a value the manager passes as
an environment variable, including systemd's `%d`-free `Environment=`/`EnvironmentFile=` and a Kubernetes
`secretKeyRef`.

By default, peryx resolves every source once when it builds the cached index and holds the value for the process
lifetime. Set `credential_refresh_secs` on an `[[index.upstream]]` source to reload a `_file` or `_env` credential
before a request after that interval has elapsed. Concurrent requests share one reload and read the current credential
without locking. Literal credentials and netrc entries cannot be refreshed.

`credential_refresh_on_unauthorized` defaults to `true`. When the upstream rejects a credential, peryx reloads its
source and replays the request once. The replay happens only when that credential generation has not already been
replaced, so a burst of rejected requests does not repeatedly read the source. `credential_failure = "fail"` rejects
requests until a later reload succeeds; `"anonymous"` retries without authentication. Credentials and OCI bearer tokens
stay isolated by configured source and origin.

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
name = "corp"
[[index.upstream]]
name = "primary"
url = "https://packages.corp.example/simple/"

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
  "origin": "https://packages.corp.example",
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

Execution is bounded to 64 argv items and 32 KiB of argv data, 1–300 seconds, 64 inherited environment names, 64 KiB of
standard output, and eight helpers across the process. The default timeout is 30 seconds. peryx clears the child
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
    "origin": "https://packages.corp.example",
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
name = "corp"
[[index.upstream]]
name = "primary"
url = "https://packages.example/simple/"
```

The traditional form matches pip and uv for a host on the scheme's default port:

```text
machine packages.example
login __token__
password pypi-token
```

Use an origin or authority machine name when the same host serves more than one credential boundary. peryx searches an
exact origin first, then `host:port`, a bare host on a default port, and `default`.

```text
machine https://packages.example:8443
login release-reader
password release-secret

machine packages.example:9443
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

A cached PyPI index or OCI registry can extend the platform trust store with a private CA and authenticate with a client
certificate. Configure the paths on each `[[index.upstream]]` source:

```toml
[[index]]
name = "corp-python"
[[index.upstream]]
name = "primary"
url = "https://packages.example/simple/"
ca_file = "/run/secrets/corp-ca.pem"
client_cert_file = "/run/secrets/peryx-client.pem"
client_key_file = "/run/secrets/peryx-client-key.pem"

[[index]]
name = "corp-images"
ecosystem = "oci"
[[index.upstream]]
name = "primary"
url = "https://registry.example"
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

peryx serves plain HTTP by default, which is the right choice for a laptop: `pip`/`uv` accept any URL, and
`docker`/`podman` trust a loopback registry (`localhost`, `127.0.0.0/8`) over HTTP with no configuration. To serve over
the network, where clients demand HTTPS, turn on TLS with one of two mutually exclusive tables. Neither is set by
default, and an unconfigured server keeps the plain-HTTP path.

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
domains = ["registry.example.com"]
contact = "admin@example.com"
cache-dir = "/var/lib/peryx/acme"  # where issued certificates are cached; default "acme-cache"
staging = false                    # true uses Let's Encrypt staging while testing
```

| Table | Key | Meaning | Default | | -------- | ----------- |
----------------------------------------------------------- | ------------ | | `[tls]` | `cert` | PEM certificate chain
| (required) | | `[tls]` | `key` | PEM private key | (required) | | `[acme]` | `domains` | Domains to request a
certificate for; reachable on port 443 | (required) | | `[acme]` | `contact` | Contact email the ACME account registers
| (required) | | `[acme]` | `cache-dir` | Where certificates and the account key are cached | `acme-cache` | | `[acme]`
| `staging` | Use the provider's staging environment | `false` |

For an `[acme]` deployment the domain's DNS must point at the server and port 443 must be reachable, since the ACME
handshake happens there. Behind a load balancer or reverse proxy that already terminates TLS, leave both tables unset
and let the proxy hold the certificate.

## `[[index]]`

Each `[[index]]` table declares one index. `name` is required; exactly one of `[[index.upstream]]`, `hosted`, or
`layers` selects the role. peryx rejects unknown keys.

| Key | Role | Meaning | Default | | ---------------------- | ------- |
-------------------------------------------------------------------- | ------------------ | | `name` | all | Identifier
other indexes reference in `layers` | (required) | | `route` | all | URL prefix the index is served under | same as
`name` | | `ecosystem` | all | Packaging format: `pypi` or `oci` | `pypi` | | `upstream` | cached | Ordered
`[[index.upstream]]` sources to cache (see below) | | | `fallback` | cached | Continue to the next source when one has
no match | `true` | | `protected` | cached | Project globs a later source may not shadow | none | | `pins` | cached |
Map of project to the source name that serves it | none | | `upstream_concurrency` | cached | Cap on concurrent upstream
fetches; `0` is unlimited and the default | `0` | | `offline` | cached | Serve this cached index from disk only |
`false` | | `prefetch` | cached | Package and artifact selection for `peryx mirror` | (see below) | | `hosted` | hosted
| `true` marks this index as a hosted store | `false` | | `volatile` | hosted | Allow delete and overwrite | `true` | |
`anonymous_read` | all | Whether a credential-less request may read this index | `[auth]` default | | `access_token` |
all | Named credentials the index accepts, with scoped grants | none | | `layers` | virtual | Ordered index names to
compose; first match per filename wins | | | `upload` | virtual | Hosted layer that receives uploads | first hosted
layer | | `policy` | all | Nested index policy table | empty | | `settings` | all | Nested table of the index
ecosystem's own settings | empty | | `webhook` | all | Signed delivery targets for upload and index-change events | none
|

A hosted index accepts uploads through its `[[index.access_token]]` grants that permit writes; there is no separate
upload-token key.

A `route` is a raw URL path prefix. It must be one or more non-empty path segments separated by `/`; each segment may
contain only ASCII letters, digits, `-`, `.`, `_`, and `~`. Startup rejects routes with a leading or trailing `/`, empty
segments, percent encoding, traversal segments, control characters, spaces, and routes whose first segment is reserved
for a Peryx endpoint: `+stats`, `+status`, `_`, `admin`, `api-docs`, `browse`, `favicon.svg`, `metrics`, `pkg`,
`search`, `stats`, or `upload`. These are the paths peryx's own API and web UI serve (the `_` segment is the `/_/oidc/*`
trusted-publishing namespace), so an index may not shadow one.

Declaring any `[[index]]` replaces the default topology, which ships a trio per ecosystem: a cached upstream, a hosted
store, and a virtual index that layers the two.

```toml
[[index]]
name = "pypi"
[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
name = "hosted"
hosted = true

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[[index]]
name = "dockerhub"
ecosystem = "oci"
[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"

[[index]]
name = "images"
ecosystem = "oci"
hosted = true

[[index]]
name = "root/oci"
ecosystem = "oci"
layers = ["images", "dockerhub"]
upload = "images"
```

Startup rejects duplicate names, duplicate routes, invalid routes, `layers` entries that name no index, `layers` that
mix ecosystems, an `upload` target that is not a hosted index, and an empty `[[index.access_token]]` `secret`. An empty
secret is a configuration error rather than a valid value: authorization compares a token secret against the Basic-auth
password on each request, so a blank string would admit any request that presents an empty password. The empty value
almost always comes from a config template whose environment variable never expanded, so peryx refuses it at load time
instead of booting into that state. To grant no upload access, configure no write-granting token.

### `[[index.upstream]]`

A cached index caches one or more upstream sources, each declared by an `[[index.upstream]]` table under it. One source
is the common case; several make an ordered route where the first source with a match answers and `fallback` controls
whether a miss continues to the next. Each source carries its own URL, credentials, and TLS, so the credential and TLS
keys below belong on the source, never on the parent `[[index]]`.

| Key | Meaning | Default | | ------------------------------------ |
---------------------------------------------------------------- | ---------- | | `name` | Identifier for the source,
unique within the index | (required) | | `url` | Upstream URL (a Simple index, or a `/v2/` registry for OCI) |
(required) | | `artifact_url` | Origin that serves artifacts when it differs from `url` | `url` | | `username` |
Basic-auth username for the upstream | (none) | | `password` | Basic-auth password; `password_file`/`password_env` read
it out | (none) | | `token` | Bearer token; takes precedence over username/password | (none) | | `token_file` | Path to
read `token` from instead of inlining it | (none) | | `token_env` | Environment variable to read `token` from instead of
inlining it | (none) | | `credential_exec` | Nested short-lived credential helper configuration | (none) | |
`credential_refresh_secs` | Minimum seconds between credential source reads | (none) | |
`credential_refresh_on_unauthorized` | Reload and replay once after credential rejection | `true` | |
`credential_failure` | Reload failure behavior: `fail` or `anonymous` | `fail` | | `ca_file` | PEM CA bundle added to
platform trust for this source | (none) | | `client_cert_file` | PEM client certificate chain; requires
`client_key_file` | (none) | | `client_key_file` | Matching unencrypted PEM client key; requires `client_cert_file` |
(none) |

```toml
[[index]]
name = "python"
fallback = true
protected = ["Internal.Pkg"]

[index.pins]
flask = "public"

[[index.upstream]]
name = "internal"
url = "https://packages.example/simple/"
username = "reader"
password_file = "/run/secrets/internal-password"

[[index.upstream]]
name = "public"
url = "https://pypi.org/simple/"
```

### `[index.policy]`

Policy rules apply to the index that owns the table. A cached-index policy filters that cache; a hosted policy filters
direct uploads and hosted-route reads; a virtual policy filters the merged index clients use. Project names are compared
after [PEP 503](https://peps.python.org/pep-0503/) normalization.

```toml
[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[index.policy]
fallback_mode = "private-first"
allow_projects = ["flask", "requests"]
block_projects = ["bad-package"]
protected_names = ["acme-secrets", "acme-*"]
allow_versions = ">=1,<3"
allow_package_types = ["wheel"]
block_package_types = ["sdist"]
allow_wheel_pythons = ["py3", "cp313"]
block_wheel_platforms = ["win_amd64"]
max_file_size_bytes = 104857600
max_project_size_bytes = 1073741824
max_accounted_bytes = 10737418240
max_projects = 500
max_versions_per_project = 100
quota_audit = false
min_release_age_secs = 604800
required_attestations = ["https://docs.pypi.org/attestations/publish/v1"]
attestation_mode = "enforce"
```

| Key | Meaning | | -------------------------- |
----------------------------------------------------------------------------- | | `fallback_mode` | PyPI virtual source
policy: `fallback`, `private-first`, or `no-fallback` | | `allow_projects` | Only these normalized projects may be
served, mirrored, or uploaded | | `block_projects` | These normalized projects are denied | | `protected_names` |
Reserved names that never fall back upstream; exact or `prefix-*` namespace | | `allow_versions` | PEP 440 specifier set
accepted for parsed distribution filenames | | `allow_package_types` | Accepted parsed file types: `wheel`, `sdist` | |
`block_package_types` | Denied parsed file types: `wheel`, `sdist` | | `allow_wheel_pythons` | Accepted wheel Python
tags, matched against each dot-compressed tag segment | | `block_wheel_pythons` | Denied wheel Python tags | |
`allow_wheel_platforms` | Accepted wheel platform tags, matched against each dot-compressed tag segment | |
`block_wheel_platforms` | Denied wheel platform tags | | `max_file_size_bytes` | Maximum file size from the Simple API
`size` field or from an uploaded file | | `max_project_size_bytes` | Maximum sum of retained file sizes for one project
detail page | | `max_accounted_bytes` | Repository quota: deduplicated bytes one repository may hold | | `max_projects`
| Repository quota: distinct project identities one repository may hold | | `max_versions_per_project` | Repository
quota: versions one project may hold | | `quota_audit` | Record a would-reject quota decision instead of denying the
write | | `min_release_age_secs` | Hide an upstream file until this many seconds past its `upload-time` | |
`required_attestations` | In-toto predicate types an upload must carry a PEP 740 attestation for | | `attestation_mode`
| `enforce` rejects a missing attestation; `audit` records it but publishes |

`min_release_age_secs` quarantines fresh upstream releases: a file whose Simple API
[`upload-time`](https://packaging.python.org/en/latest/specifications/simple-repository-api/#project-detail) is younger
than the delay is hidden from the served page, giving operators a window to catch a malicious or mistaken upload before
it reaches clients. A common baseline is a seven-day delay (`604800`). A file with no `upload-time` is hidden while the
delay is set, since its age cannot be established. The clock is the serving clock, so the file appears once enough time
passes. This is PyPI-specific and applies only to a PyPI index.

`required_attestations` makes a hosted upload carry a
[PEP 740](https://packaging.python.org/en/latest/specifications/index-hosted-attestations/) attestation for every listed
[in-toto](https://slsa.dev/spec/v1.0/provenance) predicate type. peryx evaluates the rule at the upload boundary, after
the distribution's structure and each attestation's subject binding are validated and before the file and its provenance
publish, so an upload missing a required predicate type publishes neither object. The check reads only the predicate
types the bound attestations already declared: it performs no signature, certificate, or transparency-log verification,
and asserts no publisher identity. A file uploaded with no attestations satisfies no requirement. `attestation_mode`
chooses the outcome of an unmet requirement: `enforce` (the default) returns a `403` that names the missing predicate
types without echoing bundle content, while `audit` records the same `required-attestation-audit` policy decision but
lets the upload publish, so an operator can measure coverage before turning enforcement on. Both modes persist the
decision. The rule is PyPI-specific and applies only to a PyPI index; it runs after the structural, size, and tag rules,
so a file rejected on one of those reports that denial rather than the attestation requirement.

File and project size rules require declared sizes. A file without `size` is denied by `max_file_size_bytes`; a project
page with any retained file lacking `size` is denied by `max_project_size_bytes`. Active policies use the buffered
Simple-page path so file lists and [PEP 691](https://peps.python.org/pep-0691/) `versions` are filtered together before
peryx serves bytes.

`max_accounted_bytes`, `max_projects`, and `max_versions_per_project` are the repository quota. An OCI index enforces
them on hosted pushes: a blob, mount, or manifest reserves capacity before it becomes discoverable and is refused with a
`403 DENIED` naming the crossed counter when it would exceed a limit, charging a deduplicated digest once per
repository. `quota_audit = true` records a would-reject decision and admits the push, so an operator can observe
projected enforcement before turning it on. Setting none of the three limits leaves accounting off. See
[Repository quotas](@/core/quotas.md) for the accounting model. PyPI enforcement of these keys is forthcoming.

`protected_names` reserves private names against dependency confusion. peryx refuses a reserved name on the upstream
mirror path only: a hosted member still serves it and accepts uploads for it, but a request the local members cannot
answer fails instead of reaching the public index. That closes the gap a missing, renamed, or deleted local package
would otherwise open. An entry is an exact name or a `prefix-*` namespace rule, both normalized like the incoming name
before the comparison.

`fallback_mode` controls how a PyPI virtual index chooses project candidates from its immediate hosted and cached
members:

- `fallback` is the compatibility default. It merges every member and keeps the first record for a duplicate filename,
  so hosted and upstream files with different names remain visible together.
- `private-first` serves only hosted candidates when both source classes contain the normalized project. It uses the
  cached candidates only when the hosted members contain no files, and records a structured `policy_decision` security
  event for each collision.
- `no-fallback` does not query an immediate cached member. A project with no hosted candidates returns a structured
  `403` policy denial instead of an empty or upstream page.

Protected names take precedence over all three modes: hosted members can serve a protected name, but cached members do
not query it upstream. The comparison uses the same PEP 503 normalization as project routing, so
`acme-pkg`/`acme_pkg`/`acme.pkg` select one policy decision. A nested virtual member uses its own mode; configure that
member too when its boundary must enforce the same rule.

Leaving `fallback_mode` unset preserves existing filename-level merging. An unknown value or use on an OCI index is a
startup error. The setting governs candidates returned by this server, not indexes a client adds to its own config;
pip's `--extra-index-url`, uv source overrides, and nested virtual indexes with their own mode remain separate trust
boundaries. Use `protected_names` for private names that must stay blocked even while absent or renamed.

`allow_projects`, `block_projects`, `protected_names`, `max_file_size_bytes`, `max_project_size_bytes`,
`max_accounted_bytes`, `max_projects`, `max_versions_per_project`, and `quota_audit` are ecosystem-neutral and apply to
an OCI index too, matching on image name, blob size, and repository quota: a blocked image is hidden on reads and
refused on push, a layer or manifest over the size limit is refused, and a push over a repository quota is refused. The
rest of the keys above cover fallback selection, version specifiers, package types, and wheel tags. These are
Python-specific ([PEP 440](https://packaging.python.org/en/latest/specifications/version-specifiers/) versions,
wheel/sdist types, wheel tags) and have no OCI counterpart, so they are implemented in the PyPI ecosystem crate and
apply only to a PyPI index. Each ecosystem contributes its own matchers to the same neutral `[index.policy]` engine
through a rule trait.

### `[index.settings]`

Settings the index's ecosystem defines for itself. The keys belong to the ecosystem, not to this layer: peryx carries
the table to the ecosystem of the index that owns it and compiles it there, so a key that ecosystem does not know is a
startup error.

PyPI defines no settings, so `[index.settings]` on a PyPI index fails to start. OCI defines `library_prefix` on a cached
index, which decides how that index spells a repository name when it asks its upstream for it. Its values, and what each
one rewrites, are in [OCI index settings](@/ecosystems/oci/reference/settings.md).

### `[[index.access_token]]`

Each `[[index.access_token]]` table adds one named credential the index accepts, with a grant scoped to some projects
and actions. Put these under the hosted index that stores the writes. The [access model](@/core/authentication.md)
covers the grammar; the keys are:

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

| Key | Meaning | Default | | ------------- |
---------------------------------------------------------------------------- | ---------- | | `name` | Subject the token
authenticates as; unique per index | (required) | | `secret` | Password a client presents as its Basic password |
(required) | | `secret_file` | Path to read `secret` from instead of inlining it | (none) | | `projects` | Project globs
the token may act on; `*` matches any run of characters | `["*"]` | | `actions` | Any of `read`, `write`, `delete`; at
least one | (required) | | `expires_at` | [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) time after which it stops |
never |

A token needs exactly one of `secret` and `secret_file`. Write and delete are enforced now; a `read` grant records a
read policy that the forthcoming read challenge will enforce.

### `[index.prefetch]`

Cached indexes can declare defaults for `peryx mirror plan`, `peryx mirror sync`, and `peryx mirror verify`. Core keeps
this table opaque and passes it to the selected ecosystem plugin. The plugin owns its keys, defaults, validation, and
CLI override rules. An unsupported key fails when the mirror command compiles the selection.

- [PyPI mirror configuration](@/ecosystems/pypi/reference/mirroring.md)
- [OCI mirror configuration](@/ecosystems/oci/reference/mirroring.md)

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
`X-Forwarded-Proto` for public API links and OCI token realms, whether or not `enabled` is true. Peryx uses the socket
peer when the trusted client-address suffix is malformed or the chain contains trusted addresses throughout. It treats
IPv4-mapped IPv6 addresses as their IPv4 equivalents. Leave the list empty for direct deployments. Exclude client
networks and intermediaries that accept caller-supplied forwarding headers.

Clients can change a Basic username or bearer value without changing buckets when both values resolve to the same
principal. peryx groups rotated invalid `Authorization` values under the peer IP.

| Setting | Meaning | Default | | ----------------- | -------------------------------------------------------- | -------
| | `enabled` | Install the HTTP request limiter | `false` | | `max_clients` | Maximum client/class buckets kept in
memory | `8192` | | `trusted_proxies` | IPv4 and IPv6 networks allowed to set forwarding headers | `[]` |

Each route class is a sub-table with `requests` and `window_secs`:

| Table | Route class | Default | | ----------------------- | ----------------------------------------------- |
-------------- | | `[rate_limit.listing]` | Project listing and detail pages | `600` / `60s` | | `[rate_limit.metadata]`
| PEP 658/714 `.metadata` siblings | `1200` / `60s` | | `[rate_limit.artifact]` | Artifact downloads and archive
inspection | `300` / `60s` | | `[rate_limit.upload]` | Upload, yank, restore, and delete requests | `60` / `60s` | |
`[rate_limit.admin]` | Status, stats, metrics, and discovery endpoints | `120` / `60s` |

A request's class follows its method and its path. `POST`, `PUT`, `PATCH`, and `DELETE` count against `upload`. `GET`,
`HEAD`, and `OPTIONS` are reads, classed by the path they hit: a manifest or artifact `HEAD` shares the `artifact`
budget, a project listing the `listing` budget, a `.metadata` sibling the `metadata` budget, and a status or discovery
call the `admin` budget. That split matters for OCI, where a client sends a `HEAD` on every manifest and blob before it
pulls; charging those reads against the strict `upload` budget would let a routine pull drain it and start drawing `429`
rejections.

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
name = "pypi"
upstream_concurrency = 4
[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"
```

## `[jobs]`

The `[jobs]` table controls the node-local background work peryx runs on a timer: reclaiming expired process resources
and revalidating stale cached pages. Each ecosystem is swept on its own, so independent repositories run together while
one repository never sweeps itself twice at once.

```toml
[jobs]
mode = "none"
```

| Key | Meaning | Default | | ------ | ---------------------------------------------------------- | ------- | | `mode` |
`local` runs maintenance on this node; `none` runs nothing | `local` |

`mode = "none"` starts no scheduler, timer, or worker, which suits a node fronted by an external maintenance runner or
one that should only serve. A [read replica](@/core/high-availability.md) runs no maintenance regardless of this
setting.

### Schedules

By default a node runs cache maintenance once a minute. Replace that with an explicit `[[jobs.schedule]]` array to
choose which jobs run and how often. Each entry names one registered job and a positive interval in seconds; peryx
rejects a non-positive interval at startup, naming the schedule's index (`jobs schedule [0]`).

```toml
[[jobs.schedule]]
job = "cache_maintenance"
interval_secs = 300

[[jobs.schedule]]
job = "catalog_sync"
interval_secs = 21600
repository = "pypi"
max_projects = 10000
concurrency = 4
timeout_secs = 900
```

| Key | Meaning | Default | | --------------- | ------------------------------------------------------------ |
---------- | | `job` | `cache_maintenance`, `catalog_sync`, or `dc_copy` | (required) | | `interval_secs` | Seconds
between runs, must be positive | (required) | | `repository` | Cached online PyPI index for `catalog_sync` | (required)
| | `source` | Named upstream to use instead of repository routing | routing | | `max_projects` | Maximum projects
refreshed per run; range `1..=100000` | `10000` | | `concurrency` | Requests or copies in flight; range `1..=32`
(`1..=64` copy) | `4` / `8` | | `timeout_secs` | Whole-run wall-time limit; range `1..=86400` | `900` |

`cache_maintenance` reclaims expired process resources and revalidates stale cached pages, fanning out one run per
installed ecosystem so independent repositories sweep together while one repository never sweeps itself twice at once.

`catalog_sync` refreshes the repository's remote root and then a canonical, bounded slice of project metadata. It does
not download distributions. The same repository cannot run two catalog syncs at once, while different repositories may
run within the node-local worker limits. Cancellation stops admitting project requests; completed root and project
generations remain valid because each source document publishes atomically.

`dc_copy` copies the filesystem blobs the local data center still owes from its peers, so each data center keeps its own
verified copy of every artifact a peer serves. It reads the copy backlog from the placement ledger, pulls each owed
digest from a verified peer over the replication transport, and records the local placement. It runs only on a
filesystem backend in a `dc` or `ha` group whose roster names this node and at least one peer data center, and it
accepts only `concurrency` (copies in flight, range `1..=64`, default `8`). The copy is fenced by the ownership group's
cluster term, so a node with no live consensus term copies nothing.

One bounded timer drives every schedule, so a large set costs no per-tick scan. When a tick arrives while the same job's
previous run is still going, peryx skips it rather than queueing it, and counts the skip in the job metrics. Pick an
interval longer than a sweep takes, and stagger it clear of your peak request hours so maintenance and traffic do not
contend for upstream bandwidth.

The timer keeps no durable state. On restart it sets each schedule's next run one full interval after startup and drops
the occurrences missed while the process was down rather than replaying them as a backlog.

Run the same typed job once with `peryx job run --repository pypi`. The command accepts the schedule's `source`,
`max-projects`, `concurrency`, and `timeout-secs` controls, prints processed and changed counts, and writes the same
durable job history as a scheduled run. `peryx job list` and `peryx job show <id>` inspect that history. A failed run's
error begins with a stable category such as `retryable_upstream`, `retryable_timeout`, `upstream`, `catalog_sync`, or
`project_sync`; automation can retry only the retryable categories.

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

| Key | Meaning | Default | | --------------------------- |
----------------------------------------------------------------- | ------------ | | `backend` | `filesystem` or `s3` |
`filesystem` | | `endpoint` | Base URL of the S3-compatible service (http or https) | (required) | | `bucket` | Bucket
that holds the blobs | (required) | | `region` | Signing region | (required) | | `prefix` | Key prefix inside the
bucket; blobs land at `<prefix>/sha256/...` | (none) | | `path_style` | `true` for path-style addressing (MinIO);
`false` virtual-hosted | `false` | | `timeout_secs` | Per-request timeout, in seconds | `30` | | `max_retries` | Retries
for a transient transport or 5xx/429 response | `3` | | `multipart_threshold_bytes` | Objects at or below this size
upload in one `PUT` | `16777216` | | `part_size_bytes` | Multipart part size, from 5 MiB through 5 GiB | `16777216` | |
`upload_concurrency` | Parts uploaded at once during a multipart upload | `4` | | `conditional_writes` | Endpoint
enforces `If-None-Match` create-if-absent writes | `true` | | `checksum_writes` | Endpoint validates the SHA-256
checksum sent with each write | `true` |

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
later write of the same content observes the existing digest key. `peryx backup` snapshots the `[blob]` selection
without credentials so a restore points at the same bucket; the archive omits object contents. Configure bucket-level
versioning or replication on the object store.

## `[availability]`

The `[availability]` table picks the runtime availability contract this node promises for authoritative mutations. Its
`mode` selects one of `none`, `dc`, or `ha`, whose acknowledgement guarantees the
[availability contracts](@/core/availability-contracts.md) fix. An omitted table, and an explicit `mode = "none"`,
resolve to the same single-node configuration, so a zero-config deployment carries no availability state at all. To size
and stand up each mode's shape, see [availability deployment and sizing](@/core/availability-deployment.md).

```toml
[availability]
mode = "none"
```

`none` is one writer with local durability and operator-driven [failover](@/core/high-availability.md): peryx opens no
replication client, route, or task. `dc` and `ha` select distributed coordination; each needs a
`[availability.replication]` role that carries it, so peryx rejects a `dc` or `ha` mode with no role, and rejects a
`[availability.replication]` role under `none`, naming the `availability` field.

```toml
[availability]
mode = "dc"

[availability.replication]
role = "primary"
source = "writer-a"
token_file = "/run/secrets/replication-token"
```

| Key | Meaning | Default | | ------ | --------------------- | ------- | | `mode` | `none`, `dc`, or `ha` | `none` |

The nested `[availability.replication]` table declares this node's replication role. `role = "primary"` serves the
replication journal other nodes copy; `role = "replica"` follows a primary and, like `read_only`, refuses client
mutations. peryx rejects an unknown key in either table, naming the offending field.

| Key | Role | Meaning | Default | | -------------------- | ------- |
--------------------------------------------------------------- | ---------- | | `role` | both | `primary` or `replica`
| (required) | | `source` | primary | This writer's stable name in the replication journal | (required) | | `upstream` |
replica | URL of the primary this replica follows | (required) | | `token` | both | Shared replication credential,
inline | (none) | | `token_file` | both | Path to read `token` from instead of inlining it | (none) | |
`poll_interval_secs` | replica | Seconds between change-journal polls, must be positive | `1` | | `page_size` | replica
| Changes fetched per poll, positive and within the primary limit | `100` |

A role needs exactly one of `token` or `token_file`; setting both, or neither, is rejected. Keep the credential out of
the config file with `token_file`, the path to a mounted Docker or Kubernetes secret or a systemd credential, which
peryx reads at startup and never logs. A configuration snapshot (`peryx backup`) preserves a `token_file` as its path
and never resolves the secret behind it into the manifest.

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
dc = "dc-east-1"
address = "https://a.internal:8443"
role = "writer"

[[availability.member]]
node = "replica-b"
dc = "dc-east-2"
address = "https://b.internal:8443"
role = "replica"
```

The roster is validated alongside the `[availability.replication]` role. Each `[[availability.member]]` table declares
one member.

| Key | Meaning | Default | | --------- | -------------------------------------------------------------- |
------------------------ | | `group` | The group identity the roster belongs to | (required with a roster) | | `node` |
The member's stable identity, unique within the group | (required) | | `dc` | The datacenter the member runs in, unique
within the group | (required) | | `address` | The address peers reach the member on, unique within the group |
(required) | | `role` | `writer` or `replica` | (required) |

In `ha` mode each node also sets `node_identity` to its own member's `node` value, so the ownership consensus runs under
that node's own voter identity. This is distinct from `writer_identity`, which names the one writer every node claims
and follows on the metadata plane and is therefore identical across the group: deriving the consensus identity from
`writer_identity` would make every node share the writer's voter, so no genuine multi-voter group would form and a home
failure could not transfer authority to a survivor.

peryx validates the group at startup and refuses to serve on any violation: a blank or duplicated `group`, `node`, `dc`,
or `address`; a `node` that reuses the `group` identity; anything other than exactly one `writer`; or a group with no
configured replica. It never probes a member's `address`, so an unreachable configured peer is a valid topology, not a
configuration error. A roster requires `dc` or `ha` mode; declaring one under `none` is rejected, naming the
`availability` field.

## `[auth]`

The `[auth]` table holds the access settings every index shares: the signing key of peryx's token realm, the lifetime of
a minted token, and the default each index's `anonymous_read` takes. All keys are optional.

```toml
[auth]
signing_key_file = "/run/secrets/peryx-signing-key"
token_ttl_secs = 300
default_anonymous_read = false
oidc_audience = "https://packages.example/_/oidc"
```

| Key | Meaning | Default | | ------------------------ |
------------------------------------------------------------------------------------ | ------- | | `signing_key` |
Secret peryx signs its own tokens with | (none) | | `signing_key_file` | Path to read `signing_key` from instead of
inlining it | (none) | | `token_ttl_secs` | Lifetime of a minted token, in seconds; must be positive and at most 86400
(one day) | `300` | | `default_anonymous_read` | What an index's `anonymous_read` defaults to when the index omits it |
`true` | | `oidc_audience` | Audience external CI identity tokens must carry | `peryx` |

Set at most one of `signing_key` and `signing_key_file`. peryx reads the key at startup and uses it to mint OCI and
trusted-publishing tokens whose maximum lifetime is `token_ttl_secs`. `default_anonymous_read = false` sets the
anonymous-read default once instead of adding a flag to each index. Each `[[auth.trusted_publisher]]` requires `id`,
`issuer`, `repository`, `subject`, and a non-empty `projects` list; its `claims` table is optional. The repository is a
configured writable PyPI index name. See [publish from CI identities](@/ecosystems/pypi/guides/trusted-publishing.md)
for the provider contract and examples.

Each `[[auth.ldap_provider]]` configures one named StartTLS directory and optional exact group-to-role mappings. It
supports direct user DNs and service-account search followed by a user bind. Provider URLs, attributes, trust files,
password sources, timeouts, and the total connection bound are listed under
[LDAP providers](@/core/authentication.md#ldap-providers). Configuring a provider constructs the login service but does
not add an HTTP login route or browser session.

## `[[index.webhook]]`

Put webhook tables under the index that should emit them. A target on a virtual index receives events for requests made
through the virtual-index route; the payload also names the hosted layer that stored the change.

```toml
[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[[index.webhook]]
name = "ci"
url = "https://ci.example/hooks/peryx"
secret_env = "PERYX_WEBHOOK_SECRET"
events = ["upload", "delete", "restore"]
```

| Key | Meaning | Default | | ------------ |
------------------------------------------------------------------------------------------------- | ------- | | `name` |
Stable target name used in delivery logs | | | `url` | HTTP or HTTPS endpoint that receives JSON payloads; credentials,
query, and fragment are rejected | | | `secret` | Literal HMAC signing secret | | | `secret_env` | Environment variable
that contains the HMAC signing secret | | | `events` | Event names to send; omit or leave empty for all supported event
names | all |

Use one of `secret` or `secret_env`. Supported event names are `upload`, `yank`, `unyank`, `delete`, `restore`,
`promote`, `project-status`, and `management`. Peryx emits `upload`, `yank`, `unyank`, `delete`, and `restore` from the
write endpoints in this release; the other names reserve the contract for management surfaces that use this runtime.

Peryx stores pending deliveries in the metadata database and sends them outside the request path. Delivery does not
follow redirects, so a `3xx` response counts as a failed attempt rather than reposting the signed payload to a location
the target picks. Transient failures retry up to five attempts with capped backoff of 5, 15, 45, and 135 seconds: a
`5xx` response, `408 Request Timeout`, `429 Too Many Requests`, a redirect, or a transport error. Any other `4xx`
response cannot succeed on a repeat, so the delivery fails at once rather than spending its remaining attempts. The
delivery log stores the payload, target name, attempt count, next retry time, response status, and last error. It does
not store webhook secrets.

## `[log]`

| Key | Values | Default | | -------- |
\-----------------------------------------------------------------------------------------------------------------------------------------------------------
| -------- | | `level` | a
[`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):
`error` ... `trace`, per-module filters | `info` | | `format` | `pretty`, `json` | `pretty` | | `sink` | `stdout`,
`file`, `journald`, `syslog` | `stdout` | | `file` | path, required when `sink = "file"` | (none) |

The flags `--log-level`, `--log-format`, `--log-sink`, `--log-file`, `-v`, and `-vv` override these, as do the
`PERYX_LOG_LEVEL`, `PERYX_LOG_FORMAT`, `PERYX_LOG_SINK`, and `PERYX_LOG_FILE` variables (below the flags in precedence).
