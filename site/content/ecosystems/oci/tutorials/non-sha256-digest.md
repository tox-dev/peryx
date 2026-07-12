+++
title = "Pull a manifest addressed by sha512"
description = "Stand up a stand-in registry that advertises a sha512 content digest, proxy it through peryx, and watch peryx accept, store, and serve the manifest under its own sha256."
weight = 6
+++

Most registries content-address with sha256, but the
[image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests) allows others,
and a registry may advertise its `Docker-Content-Digest` in sha512. This tutorial makes that case concrete: you run a
tiny stand-in upstream that serves a manifest under a sha512 digest, proxy it through a cached peryx index, and watch
peryx accept it, where it once returned `502`. It takes about ten minutes and builds on
[getting started](@/ecosystems/oci/tutorials/getting-started.md).

## Run a stand-in registry that uses sha512

A real registry keys on sha256, so to see the sha512 path you serve a manifest yourself. This stub answers the `/v2/`
version check and serves one manifest, advertising its sha512 digest in the header a client verifies. Save it as
`upstream.py`:

```python
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer

MANIFEST = b'{"schemaVersion":2,"config":{}}'
MEDIA_TYPE = "application/vnd.oci.image.manifest.v1+json"
SHA512 = "sha512:" + hashlib.sha512(MANIFEST).hexdigest()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/v2/":
            self.send_response(200)
            self.end_headers()
        elif self.path.startswith("/v2/demo/manifests/"):
            self.send_response(200)
            self.send_header("Content-Type", MEDIA_TYPE)
            self.send_header("Docker-Content-Digest", SHA512)
            self.end_headers()
            self.wfile.write(MANIFEST)
        else:
            self.send_response(404)
            self.end_headers()


HTTPServer(("127.0.0.1", 5000), Handler).serve_forever()
```

Run it and leave it going:

```shell
python3 upstream.py   # serving http://127.0.0.1:5000
```

## Proxy it through peryx

Point a cached index at the stub. There is nothing to configure for the digest algorithm; it is the default behavior.
Save this as `peryx.toml`:

```toml
# peryx.toml
[[index]]
name = "reg"
route = "reg"
ecosystem = "oci"
cached = "http://127.0.0.1:5000"
```

Start peryx in a second terminal:

```shell
peryx serve --config peryx.toml   # listening on 127.0.0.1:4433
```

## Pull the tag and read the digest

Pull the manifest through the `reg` route. The stub advertises sha512; peryx fetches the bytes, hashes them under its
own sha256, and serves them:

```shell
curl -si http://127.0.0.1:4433/v2/reg/demo/manifests/latest
```

The response is `200 OK`, and its `Docker-Content-Digest` is peryx's canonical sha256, not the sha512 the stub sent:

```text
HTTP/1.1 200 OK
content-type: application/vnd.oci.image.manifest.v1+json
docker-content-digest: sha256:fc6b27d31f093fca2791259bc5f1f885b0616677300f02a729ff7a782d4325fc
```

That sha256 is the digest to pin an image by, and the one a client verifies the bytes against. Before peryx accepted a
non-sha256 advertisement, this same pull compared the sha512 header to the computed sha256, read the inequality as a
corrupted download, and returned `502` with nothing cached.

## Pull by the sha512 digest

A client that already holds the upstream's sha512 digest can pull by it directly. peryx serves the bytes under the
digest you asked for and echoes it back:

```shell
curl -si http://127.0.0.1:4433/v2/reg/demo/manifests/sha512:$(printf '%s' '{"schemaVersion":2,"config":{}}' | sha512sum | cut -d' ' -f1)
```

The `docker-content-digest` on that response is the `sha512:` value from the request, while the cache still keys the
bytes on sha256 underneath.

## What you saw

peryx stored one manifest, addressed by the sha256 of its exact bytes, and served it whether the request or the upstream
named it by sha256 or sha512. It verifies the sha256 it computes and trusts the algorithm it cannot recompute, so a
registry that content-addresses with sha512 works through peryx without any special configuration. For the full rules,
including where the relaxation stops, see [content digest algorithms](@/ecosystems/oci/reference/content-digests.md).
