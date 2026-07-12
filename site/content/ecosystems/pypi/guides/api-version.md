+++
title = "Diagnose a mirror that reports api-version 1.0"
description = "Find why a peryx mirror advertises Simple api-version 1.0 instead of 1.4: its upstream declares no PEP 700 version, or a virtual layer caps the merged page, and what that means for the versions and size fields."
weight = 9
+++

A client expected [PEP 700](https://peps.python.org/pep-0700/)'s `versions` and `size` fields, but your mirror's Simple
JSON reports `meta.api-version` `1.0` and the fields are missing. peryx advertises `1.0` on purpose: it serves the
version the payload guarantees, and `1.0` means it cannot promise those fields. This guide finds the layer responsible.
The examples assume peryx at `http://127.0.0.1:4433`.

## Confirm what peryx advertises

Read the served version for the project:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/{route}/simple/{project}/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

`1.4` means peryx guarantees `versions` and `size`. `1.0` means it does not, and the two causes below are the only ways
it gets there.

## Cause 1: the upstream declares no PEP 700 version

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

## Cause 2: a virtual layer caps the merged page

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

## What api-version 1.0 means for your client

A `1.0` page is a valid page. It is not missing data that peryx should have sent; it is a page whose format never
guaranteed `versions` or `size` in the first place. Adjust the client rather than the mirror:

- Treat per-file `size` as optional. Read it when present, fall back to a `Content-Length` from a `HEAD` on the file, or
  to no size at all.
- Do not rely on a top-level `versions` array. Derive the release set from the filenames on the page instead.

If the client requires PEP 700 guarantees, point it at a route whose every layer advertises `1.1+`, and it will read
`1.4` with both fields present.

## Related

- The exact mapping from upstream version to served version:
  [the advertised Simple API version](@/ecosystems/pypi/reference/api-version.md)
- Reproduce both outcomes from scratch:
  [watch the advertised version follow the upstream](@/ecosystems/pypi/tutorials/api-version.md)
- Why peryx lowers the version instead of over-advertising:
  [an honest Simple API version](@/ecosystems/pypi/api-version.md)
