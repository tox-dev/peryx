+++
title = "Log in and push with a scoped token"
description = "Turn on the Bearer token realm, docker login against peryx, and push an image with a token scoped to one set of repositories."
weight = 4
+++

In this tutorial you turn a hosted OCI index into one that validates `docker login` and hands out repository-scoped
tokens. You give the index a token that may push under `team/*`, log in with it, push an image the scope covers, and
watch a push outside the scope get refused. It takes about ten minutes and builds on
[getting started](@/ecosystems/oci/tutorials/getting-started.md).

## Configure the realm

The realm needs a signing key and an index with a scoped credential. Save this as `peryx.toml`:

```toml
# peryx.toml
host = "127.0.0.1"
port = 4433
data_dir = "peryx-data"

[auth]
signing_key = "change-me-to-a-long-random-string"

[[index]]
name = "team"
route = "team"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "ci"
secret = "ci-secret"
projects = ["team/*"]
actions = ["read", "write"]
```

The `[auth] signing_key` turns the token realm on. The `[[index.access_token]]` names one credential, `ci`, that may
read and write any repository matching `team/*`. In production keep the key and the secret in files with
`signing_key_file` and `secret_file`; see
[keep a secret out of the config file](@/core/control-access.md#keep-a-secret-out-of-the-config-file). peryx refuses to
start when the signing key is empty or contains only whitespace.

Start the server:

```console
$ peryx serve --config peryx.toml
```

## Log in

Point `docker login` at the registry. The username is ignored; the password is the token secret.

```console
$ docker login localhost:4433 --username ci --password ci-secret
Login Succeeded
```

Behind that one command, docker probes `GET /v2/`, reads the `WWW-Authenticate: Bearer` challenge, requests a token from
`/v2/token` with your credentials, and retries the probe with the token. A wrong password stops at the token request
with a `401`, so the login fails instead of succeeding against nothing. Try it:

```console
$ docker login localhost:4433 --username ci --password wrong
Error response from daemon: login attempt ... failed with status: 401 Unauthorized
```

## Push inside the scope

Tag an image under a `team/*` repository and push it. Docker requests a token scoped to that push, and peryx grants it
because `team/*` covers `team/app`.

```console
$ docker pull alpine:3.20
$ docker tag alpine:3.20 localhost:4433/team/app:1.0
$ docker push localhost:4433/team/app:1.0
```

The push succeeds. Pull it back to confirm the round trip:

```console
$ docker pull localhost:4433/team/app:1.0
```

The same credential governs browse and search. If you later set `anonymous_read = false`, server rendering limits the
repository page to readable data. Hydrated `/+ui` requests and `/+search` enforce the same boundary, so an anonymous
browser cannot disclose the repository through another route.

## Watch a push outside the scope get refused

Now tag the same image under a repository the glob does not cover and push it.

```console
$ docker tag alpine:3.20 localhost:4433/other/app:1.0
$ docker push localhost:4433/other/app:1.0
...
denied: token does not grant this action
```

The token endpoint mints a token whose access to `other/app` is empty, and the resource route refuses it with `401`
`insufficient_scope`. The credential is valid, so docker does not retry; it reports the denial. To push there, widen the
token's `projects` or add a second token.

## List the registry catalog

Request the registry-wide catalog and inspect the challenge.

```console
$ curl -sI http://localhost:4433/v2/_catalog | grep -i www-authenticate
www-authenticate: Bearer realm="http://localhost:4433/v2/token",service="peryx",scope="registry:catalog:*"
```

Your `team/*` grant is too narrow for that scope. Change `projects` to `["*"]` and restart peryx. Request a catalog
token and use it.

```console
$ token=$(curl -sS -u ci:ci-secret \
    'http://localhost:4433/v2/token?service=peryx&scope=registry%3Acatalog%3A%2A' | jq -r .token)
$ curl -sS --oauth2-bearer "$token" http://localhost:4433/v2/_catalog
{"repositories":["team/app"]}
```

The explicit `*` grant proves the credential may read the whole index. peryx rejects a repository token for `_catalog`,
including one for `team/app`.

## What you built

One hosted index that validates logins and scopes a token to a set of repositories. To make its reads private too, set
`anonymous_read = false` on the index and give the token a `read` grant; see
[make an OCI index private](@/ecosystems/oci/guides/token-auth.md). For the wire details, read
[token authentication](@/ecosystems/oci/reference/token-auth.md).
