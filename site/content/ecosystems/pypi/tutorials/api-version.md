+++
title = "Watch the advertised version follow the upstream"
description = "Serve one upstream that declares Simple api-version 1.1+ and one bare PEP 503 index, fetch each project page through peryx, and see the advertised meta.api-version come back 1.4 for one and 1.0 for the other."
weight = 6
+++

In this tutorial you serve two upstreams through peryx, one that declares [PEP 700](https://peps.python.org/pep-0700/)'s
`1.1` (pypi.org) and one bare [PEP 503](https://peps.python.org/pep-0503/) HTML index that declares no version, then
read the `meta.api-version` peryx serves for each. One comes back `1.4`, the other `1.0`. Then you layer the two and
watch the merged page take the lower version. It takes about ten minutes and shows that peryx advertises the version its
payload guarantees, not a fixed number.

## Prerequisites

You need a peryx binary, Python 3, and a scratch directory. The pypi.org side needs network access; the local side runs
on your machine.

## Build a bare PEP 503 upstream

A PEP 503 index is a directory of HTML pages, one per project, with no version metadata. Create one project page:

```shell
mkdir -p up/simple/sampleproject
```

```html
<!-- up/simple/sampleproject/index.html -->
<!DOCTYPE html>
<html>
 <head>
  <title>
   Links for sampleproject
  </title>
 </head>
 <body>
  <a href="sampleproject-1.0.0-py3-none-any.whl">
   sampleproject-1.0.0-py3-none-any.whl
  </a>
 </body>
</html>
```

There is no `pypi:repository-version` meta tag, so this index promises neither `versions` nor `size`. Serve the tree:

```shell
python3 -m http.server 8000 --directory up
```

Leave it running and use another terminal.

## Point peryx at both upstreams

Write a config with two cached indexes, pypi.org and the local tree:

```toml
# peryx.toml
data_dir = "peryx-data"

[[index]] # declares api-version 1.1 or newer
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]] # a bare PEP 503 HTML tree: no version declared
name = "local"
cached = "http://127.0.0.1:8000/simple/"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Use a third terminal for the requests.

## Read the version pypi.org yields

Fetch `sampleproject` through the pypi.org-backed route and print only the advertised version:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/pypi/simple/sampleproject/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

It prints `1.4`. pypi.org declares `1.1` or newer, so its pages carry PEP 700's `versions` and `size`. peryx passes them
through and keeps its `1.4` ceiling.

## Read the version the bare index yields

Now fetch the same project through the local route:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/local/simple/sampleproject/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

It prints `1.0`. The bare HTML page declared no version, so it promises neither field. peryx serves `1.0` rather than
labelling the page `1.4` and implying fields it cannot guarantee.

## Layer them and watch the lower version win

Add a virtual index that stacks both layers, and restart peryx:

```toml
[[index]] # uploads-free stack: hosted-style precedence, both upstreams
name = "both"
layers = ["local", "pypi"]
```

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/both/simple/sampleproject/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

It prints `1.0`. Both layers carry `sampleproject`, and the `local` layer serves `1.0`, so the merged page takes the
lower version. A virtual index is only as capable as its weakest layer: one pre-PEP 700 layer caps the whole page.

## What you saw

The same project came back `1.4` from pypi.org, `1.0` from a bare PEP 503 index, and `1.0` from a stack that included
the bare index. peryx read the version each upstream declared, mapped `1.1+` to its `1.4` ceiling and anything below to
`1.0`, and took the weakest layer for the virtual route. The advertised version tracks what the payload guarantees, so a
client that trusts `1.4`'s `versions` and `size` is never handed a page that omits them.

## Where next

- The full mapping, including how a non-`1` major is handled:
  [the advertised Simple API version](@/ecosystems/pypi/reference/api-version.md)
- Diagnose a real mirror that reports `1.0`:
  [diagnose a mirror that reports api-version 1.0](@/ecosystems/pypi/guides/api-version.md)
- Why peryx derives the version instead of asserting it:
  [an honest Simple API version](@/ecosystems/pypi/api-version.md)
