+++
title = "Watch a slashless URL redirect"
description = "Request a Simple index and project URL without the trailing slash and watch peryx answer with a 301 to the canonical, PEP 503-normalized slashed URL."
weight = 4
+++

In this tutorial you send a few Simple API requests by hand, drop the trailing slash each time, and watch peryx point
you at the canonical URL instead of failing. It takes about five minutes and needs nothing but `curl` against a running
peryx.

The [Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) URLs end in a slash: the
index is `.../simple/` and a project is `.../simple/{project}/`. [PEP 503](https://peps.python.org/pep-0503/) says a
client that asks for one of these without the slash should be sent to the slashed form rather than turned away. `pip`
and `uv` already append the slash, so you will rarely see this in normal use; this tutorial makes it visible.

## Start peryx

The read path needs no configuration. Start the server on its default route `root/pypi`:

```shell
peryx serve
```

It listens on `http://127.0.0.1:4433`. Leave it running and open a second terminal for the requests below.

## Ask for the index without the slash

Use `curl -i` so you see the status line and headers, and stop it from following the redirect for now:

```shell
curl -i http://127.0.0.1:4433/root/pypi/simple
```

peryx answers with a `301`, not a page:

```http
HTTP/1.1 301 Moved Permanently
location: /root/pypi/simple/
```

The `Location` header carries the canonical URL: the same path with the trailing slash restored. Nothing else about the
request changed.

## Ask for a project without the slash

Now request a project, again without the slash, and use a mixed-case name with a dot in it:

```shell
curl -i http://127.0.0.1:4433/root/pypi/simple/Flask.Test
```

```http
HTTP/1.1 301 Moved Permanently
location: /root/pypi/simple/flask-test/
```

Two things happened at once. The trailing slash was restored, and the name was normalized: `Flask.Test` became
`flask-test`. PEP 503 folds a name to lowercase and collapses any run of `.`, `-`, or `_` to a single `-`, so the
redirect lands on the one canonical spelling of the project rather than a variant.

## Let curl follow the redirect

Add `-L` and `curl` follows the `Location` to the real page:

```shell
curl -iL http://127.0.0.1:4433/root/pypi/simple/flask
```

You see the `301` first, then the `200` with the project detail. Any client that follows redirects lands on the page in
one extra round trip.

## Keep a query string across the hop

Append a query string and it survives the redirect intact:

```shell
curl -i "http://127.0.0.1:4433/root/pypi/simple/Flask.Test?extra=1"
```

```http
HTTP/1.1 301 Moved Permanently
location: /root/pypi/simple/flask-test/?extra=1
```

The `?extra=1` rides along to the canonical URL, so a request that carried parameters does not lose them.

## What you saw

A Simple index or project URL without the trailing slash returns a `301` to the slashed, normalized form, the query
string intact, the same status pypi.org returns. To make a tool rely on this, see
[follow the trailing-slash redirect](@/ecosystems/pypi/guides/trailing-slash.md). For the exact rule and its edges, see
[trailing-slash redirects](@/ecosystems/pypi/reference/trailing-slash.md); for why peryx redirects rather than 404s, see
[why Simple URLs end in a slash](@/ecosystems/pypi/trailing-slashes.md).
