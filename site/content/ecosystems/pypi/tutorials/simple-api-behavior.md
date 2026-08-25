+++
title = "Test Simple API behavior"
description = "Test api-version derivation, gpg-sig filtering, pass-through markers, and slashless URL redirects."
weight = 5
aliases = [ "/ecosystems/pypi/tutorials/api-version/", "/ecosystems/pypi/tutorials/gpg-sig/", "/ecosystems/pypi/tutorials/slashless-url/"]
+++

Use `curl` to test upstream-derived `meta.api-version` values, `gpg-sig` filtering on content-addressed files, marker
retention on pass-through files, and canonical redirects for slashless URLs. peryx advertises only metadata supported by
the served bytes.

## Prerequisites

You need a peryx binary ([installation](@/core/start/installation.md) lists the channels), Python 3, and
[curl](https://curl.se/). Work in a scratch directory. Each part below writes its own `peryx.toml`; stop the previous
peryx before starting the next.

## Part 1: derive the advertised version

Serve two upstreams through peryx: pypi.org, which declares [PEP 700](https://peps.python.org/pep-0700/) `1.1`, and a
bare [PEP 503](https://peps.python.org/pep-0503/) HTML index with no version. Their served `meta.api-version` values are
`1.4` and `1.0`. A virtual index containing both serves `1.0`. The pypi.org request needs network access.

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

Leave it running and use another terminal. Write a config with two cached indexes, pypi.org and the local tree:

```toml
# peryx.toml
data_dir = "peryx-data"

[[index]] # declares api-version 1.1 or newer
ecosystem = "pypi"
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]] # a bare PEP 503 HTML tree: no version declared
ecosystem = "pypi"
name = "local"

[[index.upstream]]
name = "primary"
url = "http://127.0.0.1:8000/simple/"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Use a third terminal for the requests. Fetch `sampleproject` through the
pypi.org-backed route and print only the advertised version:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/pypi/simple/sampleproject/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

It prints `1.4`. pypi.org declares `1.1` or newer, so its pages carry PEP 700's `versions` and `size`. peryx passes them
through and keeps its `1.4` ceiling. Now fetch the same project through the local route:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/local/simple/sampleproject/ \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["meta"]["api-version"])'
```

It prints `1.0`. The bare HTML page declared no version, so it promises neither field. peryx serves `1.0` rather than
labelling the page `1.4` and implying fields it cannot guarantee. Add a virtual index that stacks both layers, and
restart peryx:

```toml
[[index]] # uploads-free stack: hosted-style precedence, both upstreams
ecosystem = "pypi"
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

## Part 2: filter the gpg-sig marker

Front a small static index whose files both advertise a GPG signature. peryx drops `gpg-sig` from a file moved to its
content-addressed route and retains the marker on a pass-through file. Build one project page listing two files marked
`data-gpg-sig="true"`: one anchor carries a `#sha256=` fragment and the other carries no hash.

```shell
mkdir -p static/demo
: > static/demo-1.0-py3-none-any.whl
: > static/demo-1.0.post1-py3-none-any.whl
sha=$(python3 -c "import hashlib; print(hashlib.sha256(open('static/demo-1.0-py3-none-any.whl','rb').read()).hexdigest())")
cat > static/demo/index.html <<EOF
<!DOCTYPE html>
<html><body>
<a href="../demo-1.0-py3-none-any.whl#sha256=$sha" data-gpg-sig="true">demo-1.0-py3-none-any.whl</a>
<a href="../demo-1.0.post1-py3-none-any.whl" data-gpg-sig="true">demo-1.0.post1-py3-none-any.whl</a>
</body></html>
EOF
```

Serve the directory on port 8000 and leave it running:

```shell
python3 -m http.server 8000 --directory static
```

`demo-1.0` has a `sha256`, so peryx can content-address it. `demo-1.0.post1` has none, so peryx cannot, and will leave
its URL alone. In a second terminal, point a cached index at the static server and start peryx:

```toml
# peryx.toml
[[index]] # cached: read-through cache of the static index
ecosystem = "pypi"
name = "static"

[[index.upstream]]
name = "primary"
url = "http://127.0.0.1:8000/"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Ask peryx for the project page as JSON, the form `pip` and `uv` read:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/static/simple/demo/ | python3 -m json.tool
```

Look at the two file objects. The content-addressed file and the pass-through file diverge on both `url` and `gpg-sig`:

```json
{
  "filename": "demo-1.0-py3-none-any.whl",
  "url": "/static/files/e3b0c442.../demo-1.0-py3-none-any.whl"
}
```

```json
{
  "filename": "demo-1.0.post1-py3-none-any.whl",
  "url": "http://127.0.0.1:8000/demo-1.0.post1-py3-none-any.whl",
  "gpg-sig": true
}
```

`demo-1.0` had a `sha256`, so peryx rewrote its `url` to its own `/static/files/...` route and dropped the `gpg-sig`
field: the field is gone, not `false`. `demo-1.0.post1` had no hash, so peryx left its `url` pointing upstream and kept
`gpg-sig: true`. The upstream `.asc` is still next to that upstream URL, so the marker is still true there.

The same split shows in the [PEP 503](https://peps.python.org/pep-0503/) HTML page. Fetch it and read the two anchors:

```shell
curl -s http://127.0.0.1:4433/static/simple/demo/
```

The `demo-1.0` anchor points at `/static/files/...` and carries no `data-gpg-sig`; the `demo-1.0.post1` anchor keeps its
upstream `href` and its `data-gpg-sig="true"`. Both serving surfaces agree, because both clear the marker on the same
condition. The marker follows the file URL peryx hands out, nothing else.

## Part 3: test a slashless URL redirect

Send Simple API requests without trailing slashes and inspect the canonical redirects. The read path needs no
configuration. Start the server on its default route `root/pypi`:

```shell
peryx serve
```

It listens on `http://127.0.0.1:4433`. Use `curl -i` so you see the status line and headers, and ask for the index
without the slash:

```shell
curl -i http://127.0.0.1:4433/root/pypi/simple
```

peryx answers with a `301`, not a page:

```http
HTTP/1.1 301 Moved Permanently
location: /root/pypi/simple/
```

The `Location` header carries the canonical URL: the same path with the trailing slash restored. Now request a project,
again without the slash, and use a mixed-case name with a dot in it:

```shell
curl -i http://127.0.0.1:4433/root/pypi/simple/Flask.Test
```

```http
HTTP/1.1 301 Moved Permanently
location: /root/pypi/simple/flask-test/
```

Two things happened at once. The trailing slash was restored, and the name was normalized: `Flask.Test` became
`flask-test`. PEP 503 folds a name to lowercase and collapses any run of `.`, `-`, or `_` to a single `-`, so the
redirect lands on the one canonical spelling of the project rather than a variant. Add `-L` and `curl` follows the
`Location` to the real page:

```shell
curl -iL http://127.0.0.1:4433/root/pypi/simple/flask
```

You see the `301` first, then the `200` with the project detail. Any client that follows redirects lands on the page in
one extra round trip. Finally, append a query string and it survives the redirect intact:

```shell
curl -i "http://127.0.0.1:4433/root/pypi/simple/Flask.Test?extra=1"
```

```http
HTTP/1.1 301 Moved Permanently
location: /root/pypi/simple/flask-test/?extra=1
```

The `?extra=1` rides along to the canonical URL, so a request that carried parameters does not lose them.

## Results

The advertised version came back `1.4` from pypi.org, `1.0` from a bare index, and `1.0` from a stack that included the
bare index; the `gpg-sig` marker was dropped for the file peryx content-addressed and kept for the one it passed
through; and a slashless URL returned a `301` to the slashed, normalized form. In every case peryx advertised what its
bytes guarantee: the version the payload satisfies, a signature only where an `.asc` is reachable, and the one canonical
URL for a project.

## Next steps

- The exact rules across JSON, HTML, and legacy JSON: [Simple API serving](@/ecosystems/pypi/reference/simple-api.md)
- Diagnose a real mirror or move a tool off the marker:
  [diagnose Simple API serving](@/ecosystems/pypi/guides/simple-api.md)
- Why peryx serves this way: [what peryx serves on the Simple API](@/ecosystems/pypi/serving.md)
