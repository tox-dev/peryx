+++
title = "Issue your first access token"
description = "Configure a project-scoped upload token and verify allowed and denied publications."
weight = 2
+++

Add a named upload token to a hosted index, scope it with a project glob, and publish through it with twine. The checks
use one accepted project and one rejected project. Allow ten minutes after completing
[getting started](@/core/start/getting-started.md).

HTTP Basic auth carries the token with the `__token__:<token>` convention used by pip, twine, and
[pypi.org](https://pypi.org/). No separate login step is required.

## Scoped upload token

A hosted index that a CI job publishes to. The job's token may write projects named `team-*` and nothing else, so a
mistyped or malicious upload to another name fails at the door instead of landing in your store.

## Write the topology

Save this as `peryx.toml`:

```toml
data_dir = "peryx-data"

[[index]]
ecosystem = "pypi"
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
ecosystem = "pypi"
name = "hosted"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
projects = ["team-*"]
actions = ["write"]

[[index]]
ecosystem = "pypi"
name = "root-pypi"
route = "root/pypi"
layers = ["hosted", "pypi"]
write_target = "hosted"
```

The `[[index.access_token]]` table names one credential the `hosted` index accepts. `secret` is the password a client
presents. `projects` is a list of globs, where `*` stands for any run of characters; `team-*` covers every project whose
normalized name starts with `team-`. `actions` lists what the token may do, from `read`, `write`, and `delete`.

Start peryx:

```shell
peryx serve --config peryx.toml
```

## Publish a project the token covers

Build a small package named `team-widgets` (reuse the steps from [getting started](@/core/start/getting-started.md),
changing the project name), then publish it to the virtual index's route. peryx accepts any username; the token is the
password, matching the `__token__` convention. Inject `ci-secret` as `TWINE_PASSWORD` with `TWINE_USERNAME=__token__`
through your secret environment before publishing:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ dist/*
```

The upload succeeds. peryx matched the password against the `ci` token, saw the normalized project name `team-widgets`
against the token's `team-*` glob, and stored the file in the `hosted` layer.

## Reject an out-of-scope project

Now build a package named `other-widgets` and try the same command:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ dist/*
```

This request returns `403` with `token does not grant this action`. The credential is valid, so peryx does not request
authentication again; the token has no grant for a project named `other-widgets`. Scope is enforced on the name the
upload declares, so a token cannot reach past the projects it was issued for.

## Inspect the principal rate-limit bucket

Add a one-request listing limit to `peryx.toml`, then restart peryx:

```toml
[rate_limit]
enabled = true

[rate_limit.listing]
requests = 1
window_secs = 60
```

Send two listing requests with the `ci` password and different Basic usernames:

```shell
curl -o /dev/null -w '%{http_code}\n' -u first:ci-secret http://127.0.0.1:4433/root/pypi/simple/
curl -o /dev/null -w '%{http_code}\n' -u second:ci-secret http://127.0.0.1:4433/root/pypi/simple/
```

peryx returns `200` for the first request and `429` for the second. Both credentials resolve to the named principal
`ci`, so a Basic username change keeps the bucket. peryx groups a wrong password under the source address. A client
cannot gain fresh buckets by rotating invalid `Authorization` values.

Leave `trusted_proxies` unset for this local run. Named principals use their verified subject. The proxy list controls
the address bucket for anonymous or invalid credentials and which peers may set the public origin. For a proxy
deployment, follow the
[reverse-proxy recipe](@/core/access/control-access.md#preserve-client-buckets-behind-a-reverse-proxy).

## Single-token configuration

If you want a hosted index that a single trusted token may write and delete anywhere, one `[[index.access_token]]` with
no `projects` filter covers it, granting write and delete over every project:

```toml
[[index]]
ecosystem = "pypi"
name = "hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "hosted-secret"
actions = ["write", "delete"]
```

Add a `projects` filter to that grant when one blanket credential is too much, which is the moment a scoped grant earns
its keep.

## Related

- Related recipes: [control access to an index](@/core/access/control-access.md)
- Every key and its default: [authentication and access control](@/core/access/authentication.md)
- Token storage and upstream authentication:
  [client auth versus upstream credentials](@/core/access/access-explained.md)
