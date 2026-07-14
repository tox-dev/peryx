+++
title = "Configuration"
description = "Every TOML key, flag, and default. Precedence is defaults < TOML file < environment < flags."
weight = 1
+++

peryx reads one [TOML](https://toml.io/) file, passed with `--config <path>`. A few operational settings double as flags
or `PERYX_*` environment variables, which override the file. Precedence is `defaults < TOML file < environment < flags`.

## Top level

| Setting                   | Flag              | Environment             | TOML key          | Default      |
| ------------------------- | ----------------- | ----------------------- | ----------------- | ------------ |
| Bind host                 | `--host`          | `PERYX_HOST`            | `host`            | `127.0.0.1`  |
| Bind port                 | `--port`          | `PERYX_PORT`            | `port`            | `4433`       |
| Data directory            | `--data-dir`      | `PERYX_DATA_DIR`        | `data_dir`        | `peryx-data` |
| Offline mode              | `--offline`       | `PERYX_OFFLINE`         | `offline`         | `false`      |
| Config file               | `--config` / `-c` | (n/a)                   | (n/a)             | (none)       |
| Cache freshness (seconds) | (file/env only)   | `PERYX_CACHE_TTL_SECS`  | `cache_ttl_secs`  | `300`        |
| Page cache budget (bytes) | (file/env only)   | `PERYX_HOT_CACHE_BYTES` | `hot_cache_bytes` | `268435456`  |
| Stale-on-error bound (s)  | (file/env only)   | `PERYX_MAX_STALE_SECS`  | `max_stale_secs`  | `300`        |
| Indexes                   | (file only)       | (n/a)                   | `[[index]]`       | (see below)  |
| Rate limits               | (file only)       | (n/a)                   | `[rate_limit]`    | (see below)  |

Environment variables sit between the file and flags: a `PERYX_*` value overrides the TOML file, and a flag overrides
the variable. Only scalar settings are environment-configurable. The `[[index]]` topology and `[rate_limit]` block stay
file-only, since neither maps to a flat variable. An empty variable is treated as unset. The `[log]` block also reads
variables (`PERYX_LOG_LEVEL`, `PERYX_LOG_FORMAT`, `PERYX_LOG_SINK`, `PERYX_LOG_FILE`); see [`[log]`](#log).

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

`hot_cache_bytes` is the memory budget for the transformed-page cache, where a warm request is a lookup, an expiry
check, and a memcpy. It trades memory against warm-serve speed and nothing else: every entry is re-derivable from the
cached raw page, so a smaller budget only lowers the hit rate, and `0` turns the cache off so each warm page pays its
transform again. Lower it on a memory-tight host; raise it when a few projects with very large index pages (`boto3` and
`numpy` run to megabytes of JSON) carry the traffic. The PyPI driver is the only ecosystem that populates it today.

`offline = true` disables upstream network access for configured cached indexes. Whatever an ecosystem has cached serves
from disk: PyPI project pages, [PEP 658](https://peps.python.org/pep-0658/) metadata siblings, and wheels; OCI manifests
and blobs. A cold cached-index miss returns `503`; virtual-index routes still serve any hosted layer that can answer.
Use `peryx mirror sync` before enabling offline mode on a machine that must run without network access.

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

Each `[[index]]` table declares one index. `name` is required; exactly one of `cached`, `hosted`, or `layers` selects
the role. peryx rejects unknown keys.

| Key                    | Role    | Meaning                                                               | Default            |
| ---------------------- | ------- | --------------------------------------------------------------------- | ------------------ |
| `name`                 | all     | Identifier other indexes reference in `layers`                        | (required)         |
| `route`                | all     | URL prefix the index is served under                                  | same as `name`     |
| `ecosystem`            | all     | Packaging format: `pypi` or `oci`                                     | `pypi`             |
| `cached`               | cached  | Upstream URL to cache (a Simple index, or a `/v2/` registry for OCI)  |                    |
| `username`             | cached  | Basic-auth username for the upstream                                  | (none)             |
| `password`             | cached  | Basic-auth password for the upstream                                  | (none)             |
| `password_file`        | cached  | Path to read `password` from instead of inlining it                   | (none)             |
| `token`                | cached  | Bearer token; takes precedence over username/password                 | (none)             |
| `token_file`           | cached  | Path to read `token` from instead of inlining it                      | (none)             |
| `upstream_concurrency` | cached  | Cap on concurrent upstream fetches; `0` is unlimited and the default  | `0`                |
| `offline`              | cached  | Serve this cached index from disk only                                | `false`            |
| `prefetch`             | cached  | Package and artifact selection for `peryx mirror`                     | (see below)        |
| `hosted`               | hosted  | `true` marks this index as a hosted store (implied by `upload_token`) | `false`            |
| `upload_token`         | hosted  | Sugar for one token granted write and delete over every project       | (none)             |
| `upload_token_file`    | hosted  | Path to read `upload_token` from instead of inlining it               | (none)             |
| `volatile`             | hosted  | Allow delete and overwrite                                            | `true`             |
| `anonymous_read`       | all     | Whether a credential-less request may read this index                 | `[auth]` default   |
| `access_token`         | all     | Named credentials the index accepts, with scoped grants               | none               |
| `layers`               | virtual | Ordered index names to compose; first match per filename wins         |                    |
| `upload`               | virtual | Hosted layer that receives uploads                                    | first hosted layer |
| `policy`               | all     | Nested index policy table                                             | empty              |
| `settings`             | all     | Nested table of the index ecosystem's own settings                    | empty              |
| `webhook`              | all     | Signed delivery targets for upload and index-change events            | none               |

A `route` is a raw URL path prefix. It must be one or more non-empty path segments separated by `/`; each segment may
contain only ASCII letters, digits, `-`, `.`, `_`, and `~`. Startup rejects routes with a leading or trailing `/`, empty
segments, percent encoding, traversal segments, control characters, spaces, and routes whose first segment is reserved
for Peryx endpoints such as `browse`, `stats`, `+stats`, `+status`, `api-docs`, `metrics`, and `pkg`.

Declaring any `[[index]]` replaces the default topology, which ships a trio per ecosystem: a cached upstream, a hosted
store, and a virtual index that layers the two.

```toml
[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

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
cached = "https://registry-1.docker.io"

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
mix ecosystems, an `upload` target that is not a hosted index, and an empty `upload_token = ""`. An empty token is a
configuration error rather than a valid value: authorization compares `upload_token` against the Basic-auth password on
each request, so a blank string would admit any request that presents an empty password and turn off write and delete
authentication for the index. The empty value almost always comes from a config template whose environment variable
never expanded, so peryx refuses it at load time instead of booting into that state. To configure no token, omit the key
rather than setting it to `""`.

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
min_release_age_secs = 604800
```

| Key                      | Meaning                                                                       |
| ------------------------ | ----------------------------------------------------------------------------- |
| `allow_projects`         | Only these normalized projects may be served, mirrored, or uploaded           |
| `block_projects`         | These normalized projects are denied                                          |
| `protected_names`        | Reserved names that never fall back upstream; exact or `prefix-*` namespace   |
| `allow_versions`         | PEP 440 specifier set accepted for parsed distribution filenames              |
| `allow_package_types`    | Accepted parsed file types: `wheel`, `sdist`                                  |
| `block_package_types`    | Denied parsed file types: `wheel`, `sdist`                                    |
| `allow_wheel_pythons`    | Accepted wheel Python tags, matched against each dot-compressed tag segment   |
| `block_wheel_pythons`    | Denied wheel Python tags                                                      |
| `allow_wheel_platforms`  | Accepted wheel platform tags, matched against each dot-compressed tag segment |
| `block_wheel_platforms`  | Denied wheel platform tags                                                    |
| `max_file_size_bytes`    | Maximum file size from the Simple API `size` field or from an uploaded file   |
| `max_project_size_bytes` | Maximum sum of retained file sizes for one project detail page                |
| `min_release_age_secs`   | Hide an upstream file until this many seconds past its `upload-time`          |

`min_release_age_secs` quarantines fresh upstream releases: a file whose Simple API
[`upload-time`](https://packaging.python.org/en/latest/specifications/simple-repository-api/#project-detail) is younger
than the delay is hidden from the served page, giving operators a window to catch a malicious or mistaken upload before
it reaches clients. A common baseline is a seven-day delay (`604800`). A file with no `upload-time` is hidden while the
delay is set, since its age cannot be established. The clock is the serving clock, so the file appears once enough time
passes. This is PyPI-specific and applies only to a PyPI index.

File and project size rules require declared sizes. A file without `size` is denied by `max_file_size_bytes`; a project
page with any retained file lacking `size` is denied by `max_project_size_bytes`. Active policies use the buffered
Simple-page path so file lists and [PEP 691](https://peps.python.org/pep-0691/) `versions` are filtered together before
peryx serves bytes.

`protected_names` reserves private names against dependency confusion. peryx refuses a reserved name on the upstream
mirror path only: a hosted member still serves it and accepts uploads for it, but a request the local members cannot
answer fails instead of reaching the public index. That closes the gap a missing, renamed, or deleted local package
would otherwise open. An entry is an exact name or a `prefix-*` namespace rule, both normalized like the incoming name
before the comparison.

`allow_projects`, `block_projects`, `protected_names`, `max_file_size_bytes`, and `max_project_size_bytes` are
ecosystem-neutral and apply to an OCI index too, matching on image name and blob size: a blocked image is hidden on
reads and refused on push, and a layer or manifest over the size limit is refused. The rest of the keys above cover
version specifiers, package types, and wheel tags. These are Python-specific
([PEP 440](https://packaging.python.org/en/latest/specifications/version-specifiers/) versions, wheel/sdist types, wheel
tags) and have no OCI counterpart, so they are implemented in the PyPI ecosystem crate and apply only to a PyPI index.
Each ecosystem contributes its own matchers to the same neutral `[index.policy]` engine through a rule trait.

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

| Key           | Meaning                                                                      | Default    |
| ------------- | ---------------------------------------------------------------------------- | ---------- |
| `name`        | Subject the token authenticates as; unique per index, not `upload_token`     | (required) |
| `secret`      | Password a client presents as its Basic password                             | (required) |
| `secret_file` | Path to read `secret` from instead of inlining it                            | (none)     |
| `projects`    | Project globs the token may act on; `*` matches any run of characters        | `["*"]`    |
| `actions`     | Any of `read`, `write`, `delete`; at least one                               | (required) |
| `expires_at`  | [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) time after which it stops | never      |

A token needs exactly one of `secret` and `secret_file`. Write and delete are enforced now; a `read` grant records a
read policy that the forthcoming read challenge will enforce.

### `[index.prefetch]`

Cached indexes can declare the default selection for `peryx mirror plan`, `peryx mirror sync`, and
`peryx mirror verify`. CLI flags add package selectors and override booleans or `mode` for one run.

```toml
[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[index.prefetch]
mode = "selected"
packages = ["requests>=2,<3"]
requirements = ["requirements.txt"]
include_wheels = true
include_sdists = true
python_tags = ["py3", "cp312"]
abi_tags = ["none", "abi3"]
platform_tags = ["any", "manylinux_2_28_x86_64"]
max_file_size_bytes = 524288000
metadata_only = false
```

| Key                   | Values                               | Default    |
| --------------------- | ------------------------------------ | ---------- |
| `mode`                | `selected`, `all`, `metadata-only`   | `selected` |
| `packages`            | package selectors such as `flask>=3` | `[]`       |
| `requirements`        | requirements or constraints files    | `[]`       |
| `include_wheels`      | boolean                              | `true`     |
| `include_sdists`      | boolean                              | `true`     |
| `python_tags`         | wheel Python tags                    | `[]`       |
| `abi_tags`            | wheel ABI tags                       | `[]`       |
| `platform_tags`       | wheel platform tags                  | `[]`       |
| `max_file_size_bytes` | positive integer                     | (none)     |
| `metadata_only`       | boolean                              | `false`    |

`mode = "all"` reads the upstream root Simple project list and then visits matching projects. Artifact filters apply
after a project page is fetched. `mode = "metadata-only"` implies `metadata_only = true`.

The wheel/sdist and wheel-tag keys above are the PyPI selection set and seed `peryx mirror` for a PyPI index. For an OCI
index, `packages` is the image list `peryx mirror` pulls (image references such as `library/alpine:3.19`), the same way
it seeds a PyPI index's project list; `--image <ref>` on the command line adds one-off references on top. The PyPI
wheel/sdist/wheel-tag keys do not apply to an OCI index.

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

| Setting           | Meaning                                                  | Default |
| ----------------- | -------------------------------------------------------- | ------- |
| `enabled`         | Install the HTTP request limiter                         | `false` |
| `max_clients`     | Maximum client/class buckets kept in memory              | `8192`  |
| `trusted_proxies` | IPv4 and IPv6 networks allowed to set forwarding headers | `[]`    |

Each route class is a sub-table with `requests` and `window_secs`:

| Table                   | Route class                                     | Default        |
| ----------------------- | ----------------------------------------------- | -------------- |
| `[rate_limit.listing]`  | Project listing and detail pages                | `600` / `60s`  |
| `[rate_limit.metadata]` | PEP 658/714 `.metadata` siblings                | `1200` / `60s` |
| `[rate_limit.artifact]` | Artifact downloads and archive inspection       | `300` / `60s`  |
| `[rate_limit.upload]`   | Upload, yank, restore, and delete requests      | `60` / `60s`   |
| `[rate_limit.admin]`    | Status, stats, metrics, and discovery endpoints | `120` / `60s`  |

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
cached = "https://pypi.org/simple/"
upstream_concurrency = 4
```

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
| `token_ttl_secs`         | Lifetime of a minted token, in seconds; must be positive             | `300`   |
| `default_anonymous_read` | What an index's `anonymous_read` defaults to when the index omits it | `true`  |

Set at most one of `signing_key` and `signing_key_file`. `signing_key` and `token_ttl_secs` configure the token realm
the forthcoming OCI Bearer flow mints from; peryx reads the key at startup so a deployment can stage it, and nothing
mints a token from it in this release. `default_anonymous_read = false` makes a fully private server one knob instead of
a flag per index. The full model is in [authentication and access control](@/core/authentication.md).

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

| Key          | Meaning                                                                                           | Default |
| ------------ | ------------------------------------------------------------------------------------------------- | ------- |
| `name`       | Stable target name used in delivery logs                                                          |         |
| `url`        | HTTP or HTTPS endpoint that receives JSON payloads; credentials, query, and fragment are rejected |         |
| `secret`     | Literal HMAC signing secret                                                                       |         |
| `secret_env` | Environment variable that contains the HMAC signing secret                                        |         |
| `events`     | Event names to send; omit or leave empty for all supported event names                            | all     |

Use one of `secret` or `secret_env`. Supported event names are `upload`, `yank`, `unyank`, `delete`, `restore`,
`promote`, `project-status`, and `management`. Peryx emits `upload`, `yank`, `unyank`, `delete`, and `restore` from the
write endpoints in this release; the other names reserve the contract for management surfaces that use this runtime.

Peryx stores pending deliveries in the metadata database and sends them outside the request path. A failed delivery
retries up to five attempts with capped backoff of 5, 15, 45, and 135 seconds. The delivery log stores the payload,
target name, attempt count, next retry time, response status, and last error. It does not store webhook secrets.

## `[log]`

| Key      | Values                                                                                                                                                      | Default  |
| -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `level`  | a [`tracing` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): `error` ... `trace`, per-module filters | `info`   |
| `format` | `pretty`, `json`                                                                                                                                            | `pretty` |
| `sink`   | `stdout`, `file`, `journald`, `syslog`                                                                                                                      | `stdout` |
| `file`   | path, required when `sink = "file"`                                                                                                                         | (none)   |

The flags `--log-level`, `--log-format`, `--log-sink`, `--log-file`, `-v`, and `-vv` override these, as do the
`PERYX_LOG_LEVEL`, `PERYX_LOG_FORMAT`, `PERYX_LOG_SINK`, and `PERYX_LOG_FILE` variables (below the flags in precedence).
