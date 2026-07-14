+++
title = "Issue your first access token"
description = "Give a hosted index a named token scoped to a set of projects, then publish through it and watch a project outside the scope get refused."
weight = 2
+++

In this tutorial you will add a named upload token to a hosted index, scope it to a set of projects with a glob, and
publish through it with twine. You will publish one package the token covers and watch a second one, outside the scope,
get turned away. It takes about ten minutes and builds on [getting started](@/core/getting-started.md).

The token is enforced through HTTP Basic auth: the same `__token__:<token>` convention pip and twine already use for
[pypi.org](https://pypi.org/), so no new client and no login step is involved. A hosted OCI index enforces the same
model on `docker push`; this walkthrough uses PyPI because twine needs the least setup.

## The goal

A hosted index that a CI job publishes to. The job's token may write projects named `team-*` and nothing else, so a
mistyped or malicious upload to another name fails at the door instead of landing in your store.

## Write the topology

Save this as `peryx.toml`:

```toml
data_dir = "peryx-data"

[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
projects = ["team-*"]
actions = ["write"]

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

The `[[index.access_token]]` table names one credential the `hosted` index accepts. `secret` is the password a client
presents. `projects` is a list of globs, where `*` stands for any run of characters; `team-*` covers every project whose
normalized name starts with `team-`. `actions` lists what the token may do, from `read`, `write`, and `delete`.

Start peryx:

```shell
peryx serve --config peryx.toml
```

## Publish a project the token covers

Build a small package named `team-widgets` (reuse the steps from [getting started](@/core/getting-started.md), changing
the project name), then publish it to the virtual index's route. peryx accepts any username; the token is the password,
matching the `__token__` convention:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p ci-secret dist/*
```

The upload succeeds. peryx matched the password against the `ci` token, saw the normalized project name `team-widgets`
against the token's `team-*` glob, and stored the file in the `hosted` layer.

## Watch a project outside the scope get refused

Now build a package named `other-widgets` and try the same command:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p ci-secret dist/*
```

This one returns `403` with `token does not grant this action`. The credential is valid, so peryx does not ask you to
authenticate again; the token simply holds no grant for a project named `other-widgets`. Scope is enforced on the name
the upload declares, so a token cannot reach past the projects it was issued for.

## Observe the principal rate-limit bucket

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
deployment, follow the [reverse-proxy recipe](@/core/control-access.md#preserve-client-buckets-behind-a-reverse-proxy).

## The one-token shortcut

If you want a hosted index that a single trusted token may write and delete anywhere, you do not need an
`[[index.access_token]]` table at all. The older `upload_token` key still works and stands for exactly that, one
credential granted write and delete over every project:

```toml
[[index]]
name = "hosted"
upload_token = "hosted-secret"
```

An index configured this way behaves as it did before peryx had a token model. Reach for `[[index.access_token]]` when
one blanket credential is too much, which is the moment a scoped grant earns its keep.

## Where next

- The tasks behind this walkthrough, one recipe each: [control access to an index](@/core/control-access.md)
- Every key and its default: [authentication and access control](@/core/authentication.md)
- Why peryx holds its own tokens and never forwards yours upstream:
  [client auth versus upstream credentials](@/core/access-explained.md)
