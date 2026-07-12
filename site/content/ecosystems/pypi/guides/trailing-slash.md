+++
title = "Follow the trailing-slash redirect"
description = "Make a script or tool that hits a Simple URL without the trailing slash work: follow the 301 peryx returns, and normalize project names yourself to skip the extra hop."
weight = 7
+++

You have a tool or a script that builds Simple API URLs by hand and hits `.../simple/{project}` without the trailing
slash. Against pypi.org that returns a `301` to the canonical URL; peryx returns the same `301`. This guide keeps that
code working, and shows how to avoid the extra round trip when it matters.

`pip`, `uv`, `twine`, and `poetry` already append the slash, so this only comes up in custom code: a shell loop, a
health check, a crawler, a language client that assembles URLs itself.

## Follow the redirect

The redirect is a plain `301` with a `Location` header. Any HTTP client can follow it; most need a flag or an option
turned on, since following a redirect after a non-`GET` is off by default in some clients.

{% tabs(names="curl, Python, httpie") %}

```shell
# -L follows the Location header to the slashed, normalized URL
curl -LsS http://127.0.0.1:4433/root/pypi/simple/Flask
```

%%%

```python
import httpx

# follow_redirects is off by default in httpx and requests; turn it on
resp = httpx.get("http://127.0.0.1:4433/root/pypi/simple/Flask", follow_redirects=True)
resp.raise_for_status()
```

%%%

```shell
# httpie follows redirects with --follow
http --follow GET http://127.0.0.1:4433/root/pypi/simple/Flask
```

{% end %}

Each of these lands on `/root/pypi/simple/flask/` and reads the project detail. The query string, if any, is carried
across the hop, so parameters survive.

## Normalize the name yourself to skip the hop

The redirect also normalizes the project name, so a slashless request for a non-canonical spelling costs two round
trips: the `301`, then the page. If you control the URL you build, normalize the name and append the slash yourself, and
the first request hits the page directly.

Normalization is [PEP 503](https://peps.python.org/pep-0503/): lowercase the name, then collapse every run of `.`, `-`,
or `_` to a single `-`.

{% tabs(names="Python, shell") %}

```python
import re

def normalize(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()

url = f"http://127.0.0.1:4433/root/pypi/simple/{normalize('Flask.Test')}/"
# http://127.0.0.1:4433/root/pypi/simple/flask-test/
```

%%%

```shell
name="Flask.Test"
slug=$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]' | sed -E 's/[-_.]+/-/g')
url="http://127.0.0.1:4433/root/pypi/simple/${slug}/"
# http://127.0.0.1:4433/root/pypi/simple/flask-test/
```

{% end %}

With the name already canonical and the slash in place, no redirect fires.

## Watch for a name with a slash in it

Only a single project segment is redirected. A path with an extra slash in it, such as `.../simple/some/thing`, is not a
project name and is not redirected; it falls through to a `404`. A project name never contains a slash, so this only
bites a malformed URL. Build the path from a normalized name and one trailing slash and you stay on the redirected path.

## Related

- See the redirect end to end: [watch a slashless URL redirect](@/ecosystems/pypi/tutorials/slashless-url.md).
- The exact rule and its edges: [trailing-slash redirects](@/ecosystems/pypi/reference/trailing-slash.md).
- Why peryx redirects instead of 404ing: [why Simple URLs end in a slash](@/ecosystems/pypi/trailing-slashes.md).
