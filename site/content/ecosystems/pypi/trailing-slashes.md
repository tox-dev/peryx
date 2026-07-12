+++
title = "Why Simple URLs end in a slash"
description = "The reasoning behind redirecting a slashless Simple URL instead of returning 404: PEP 503's canonical form, matching Warehouse, saving a client a wasted request, and the tie to name normalization."
weight = 3
+++

A Simple API index is `.../simple/` and a project is `.../simple/{project}/`. Both end in a slash. Ask for either
without it and peryx sends back a `301` to the slashed form rather than a `404`. This page is about why.

## The canonical URL has a slash

[PEP 503](https://peps.python.org/pep-0503/) defines the Simple API URLs with a trailing slash, and says a client that
requests a URL without it should be redirected to the version with it. The slashed URL is the canonical one; the
slashless URL is a request for a resource that lives one redirect away. Answering it with a `404` would be telling the
client the project does not exist, which is wrong: it exists, at the URL one hop over.

A redirect says the resource has a canonical location and the client should use it from now on, which is what
`301 Moved Permanently` means. A well-behaved client follows the hop, and a caching one remembers it and skips the round
trip next time.

## Matching what clients already expect

pypi.org, served by [Warehouse](https://github.com/pypi/warehouse), returns exactly this `301` for a slashless Simple
URL. Tools written against pypi.org, and the installers themselves, are built for that behavior. A cache that fronts or
stands in for pypi.org should not answer differently: a client that works against the real index should work against
peryx unchanged. Returning a `404` where pypi.org returns a `301` is the kind of difference that surfaces only in the
one script that drops the slash, and only in production.

## Saving a client a failed request

Without the redirect, a slashless request is a dead end. The client gets a `404`, and the person or tool behind it has
to notice the missing slash, add it, and try again, or worse, conclude the package is gone. The redirect turns that dead
end into a working request: the client is handed the right URL and gets the page. One request that would have failed
becomes one that succeeds, at the cost of a single extra round trip that a caching client pays only once.

## The normalization tie-in

The redirect does not only add the slash; it also normalizes the project name. PEP 503 folds a name to lowercase and
collapses any run of `.`, `-`, or `_` to a single `-`, so `Flask.Test`, `flask_test`, and `flask-test` are all the same
project. That project has one canonical page, at `.../simple/flask-test/`. A slashless request for any spelling is a
request for that one page under a non-canonical name, so the natural target of the redirect is the normalized, slashed
URL. Adding the slash and normalizing the name are the same act: routing the request to the single canonical URL for the
resource it named.

This is why the `Location` is always the canonical form, slash and normalization together, rather than the requested
path with a slash tacked on.

## See also

- Watch it happen: [watch a slashless URL redirect](@/ecosystems/pypi/tutorials/slashless-url.md).
- Make a tool rely on it: [follow the trailing-slash redirect](@/ecosystems/pypi/guides/trailing-slash.md).
- The exact rule: [trailing-slash redirects](@/ecosystems/pypi/reference/trailing-slash.md).
