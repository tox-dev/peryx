+++
title = "Diagnose Simple API serving"
description = "Work out why a mirror reports api-version 1.0, move a client off the gpg-sig marker to the sha256 the index serves, and follow or skip the trailing-slash redirect."
weight = 7
aliases = [
    "/ecosystems/pypi/guides/api-version/",
    "/ecosystems/pypi/guides/gpg-sig/",
    "/ecosystems/pypi/guides/trailing-slash/",
]
+++

Three things a client reads off a Simple page can surprise it: an api-version lower than expected, a missing GPG
signature marker, and a `301` where it sent a slashless URL. Each is peryx serving only what its bytes back up. This
guide diagnoses all three and shows what to rely on instead. The examples assume peryx at `http://127.0.0.1:4433`.

## Diagnose a mirror that reports api-version 1.0

A client expected [PEP 700](https://peps.python.org/pep-0700/)'s `versions` and `size` fields, but your mirror's Simple
JSON reports `meta.api-version` `1.0` and the fields are missing. peryx advertises `1.0` on purpose: it serves the
version the payload guarantees, and `1.0` means it cannot promise those fields.

### Confirm what peryx advertises

Read the served version for the project:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/{route}/simple/{project}/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

`1.4` means peryx guarantees `versions` and `size`. `1.0` means it does not, and the two causes below are the only ways
it gets there.

### Cause 1: the upstream declares no PEP 700 version

peryx serves `1.0` for an upstream that declared `1.0`, or that declared no version at all, such as a plain
[PEP 503](https://peps.python.org/pep-0503/) HTML index. Ask the upstream for the version it sends:

```shell
# a JSON upstream: read its own meta.api-version
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    https://upstream.example/simple/{project}/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin).get("meta", {}).get("api-version"))'

# an HTML-only upstream: look for the repository-version meta tag
curl -s https://upstream.example/simple/{project}/ | grep -i pypi:repository-version
```

If the JSON prints `None` or `1.0`, or the HTML carries no `pypi:repository-version` tag, the upstream promises neither
field, and peryx is right to serve `1.0`. peryx does not invent `versions` or `size` to reach `1.4`; it advertises the
version the upstream's bytes satisfy.

To serve `1.4`, front an upstream that declares `1.1` or higher. pypi.org does; a bare HTML mirror or an older
Artifactory may not.

### Cause 2: a virtual layer caps the merged page

A virtual index is only as capable as its weakest layer. If any layer that carries the project serves `1.0`, the merged
page drops to `1.0`, even when another layer would serve `1.4` on its own. Query each layer on its own route to find the
one holding the version down:

```shell
for route in hosted pypi; do
    printf '%s: ' "$route"
    curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
        "http://127.0.0.1:4433/${route}/simple/{project}/" \
        | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
done
```

The layer that prints `1.0` is the cap. A hosted layer whose uploads carry no recorded size, or a cached layer fronting
a PEP 503 upstream, will show up here. The merged page cannot promise a field one of its layers omits, so it takes the
lower version by design.

### What api-version 1.0 means for your client

A `1.0` page is a valid page. It is not missing data that peryx should have sent; it is a page whose format never
guaranteed `versions` or `size` in the first place. Adjust the client rather than the mirror:

- Treat per-file `size` as optional. Read it when present, fall back to a `Content-Length` from a `HEAD` on the file, or
  to no size at all.
- Do not rely on a top-level `versions` array. Derive the release set from the filenames on the page instead.

If the client requires PEP 700 guarantees, point it at a route whose every layer advertises `1.1+`, and it will read
`1.4` with both fields present.

## Rely on hashes instead of the gpg-sig marker

You have a client or a tool that read the `gpg-sig` marker (`data-gpg-sig` in HTML, `has_sig` in the legacy JSON) or
fetched a file's `.asc` signature. Through peryx that marker is now absent for the files peryx content-addresses onto
its own route, and `{file_url}.asc` on those files returns `404`.

### Stop fetching `.asc` from peryx's file route

If your tool sees the marker and fetches `{file_url}.asc`, drop that fetch for any file whose URL points at peryx's
`/{route}/files/{sha256}/{filename}` route. peryx serves the blob and the [PEP 658](https://peps.python.org/pep-0658/)
`.metadata` sibling there, never the `.asc`. The signature was never at peryx's route; it lived next to the upstream URL
that peryx replaced. The marker is now dropped so your tool does not chase a `404`.

### Verify with the hash the index serves

Integrity is what most `.asc` checks were after, and the Simple API already carries it. Every file object lists a
`hashes` map, and peryx serves a `sha256` for each content-addressed file:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/requests/ \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['files'][0]['hashes'])"
```

`pip` and `uv` already verify this hash on download; peryx verifies the digest as it streams the blob into its store. A
lockfile pins the same `sha256`, so requiring hashes in your install (`pip install --require-hashes`, a `uv.lock`, or
`--only-binary` with pinned digests) gives you tamper-evidence tied to the file bytes, without a signature.

### When the signature is required

If a policy requires the OpenPGP signature, fetch it from the origin index that holds it, not through peryx. Read the
original file URL from the upstream index's own Simple page and fetch its `.asc` there:

```shell
curl -sfO https://files.pythonhosted.org/.../requests-2.32.5-py3-none-any.whl.asc
```

Two caveats. [PyPI deprecated GPG signatures in 2023](https://blog.pypi.org/posts/2023-05-23-removing-pgp/) and no
longer serves them, so for files from pypi.org the `.asc` is gone at the source too, not only through peryx. And a
private upstream that still signs is reachable only if your network allows it; the point of peryx is often that it does
not. Weigh whether a deprecated signature is worth a direct dependency on the origin before you build it in.

## Follow the trailing-slash redirect

You have a tool or a script that builds Simple API URLs by hand and hits `.../simple/{project}` without the trailing
slash. Against pypi.org that returns a `301` to the canonical URL; peryx returns the same `301`. This keeps that code
working, and shows how to avoid the extra round trip when it matters.

`pip`, `uv`, `twine`, and `poetry` already append the slash, so this only comes up in custom code: a shell loop, a
health check, a crawler, a language client that assembles URLs itself.

### Follow the redirect

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

### Normalize the name yourself to skip the hop

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

### Watch for a name with a slash in it

Only a single project segment is redirected. A path with an extra slash in it, such as `.../simple/some/thing`, is not a
project name and is not redirected; it falls through to a `404`. A project name never contains a slash, so this only
bites a malformed URL. Build the path from a normalized name and one trailing slash and you stay on the redirected path.

## Related

- The exact rules across JSON, HTML, and legacy JSON: [Simple API serving](@/ecosystems/pypi/reference/simple-api.md)
- Why peryx serves this way: [what peryx serves on the Simple API](@/ecosystems/pypi/serving.md)
- Watch all three behaviors end to end: [observe Simple API behavior](@/ecosystems/pypi/tutorials/simple-api-behavior.md)
</content>
