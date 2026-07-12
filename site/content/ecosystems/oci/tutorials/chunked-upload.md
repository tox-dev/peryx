+++
title = "Push a blob chunk by chunk"
description = "Drive an OCI chunked blob upload by hand with curl: start a session, PATCH chunks with Content-Range, recover from a 416, cancel with DELETE, and finish with PUT, then pull a manifest an upstream advertises under sha512."
weight = 5
aliases = ["/ecosystems/oci/tutorials/non-sha256-digest/"]
+++

`docker push` and `crane push` upload a blob for you in one call, so you never see the steps underneath. This tutorial
runs those steps by hand with `curl` against a hosted index, so the chunked-upload state machine the
[distribution spec](https://github.com/opencontainers/distribution-spec) defines becomes something you can watch: you
start a session, append the blob one chunk at a time, deliberately send a chunk out of order to trigger a `416` and
recover from it, cancel a session, and finish a real upload with a digest check. Then, on the same `/v2/` surface, you
proxy an upstream that content-addresses with sha512 and watch peryx accept it. It takes about twenty minutes and builds
on [getting started](@/ecosystems/oci/tutorials/getting-started.md).

## Configure a hosted index

An upload session belongs to a hosted index, and writing to it needs the index's `upload_token`. Save this as
`peryx.toml`:

```toml
# peryx.toml
[[index]]
name = "images"
route = "images"
ecosystem = "oci"
hosted = true
upload_token = "demo-secret"
```

Start peryx and leave it running; use a second terminal for the rest:

```shell
peryx serve --config peryx.toml   # listening on 127.0.0.1:4433
```

Every request below sends `-u _:demo-secret`: peryx ignores the username and takes the token as the Basic-auth password.

## Build a blob to upload

Make a small blob in three pieces so there is something to chunk, and record its `sha256` for the commit at the end:

```shell
printf 'chunk-one-'   > part-a   # 10 bytes
printf 'chunk-two-'   > part-b   # 10 bytes
printf 'chunk-three'  > part-c   # 11 bytes
cat part-a part-b part-c > blob.bin
sha256sum blob.bin               # <hex>  blob.bin  -> 31 bytes total
```

Keep the hex from `sha256sum`; you push it as `sha256:<hex>` on the `PUT`.

## Start a session

A bare `POST` to the uploads endpoint opens a session and answers `202` with the coordinates you drive the rest of the
upload with. Capture the `Location` path it returns:

```shell
loc=$(curl -sS -u _:demo-secret -X POST -D - -o /dev/null \
  http://127.0.0.1:4433/v2/images/blob-demo/blobs/uploads/ \
  | tr -d '\r' | awk 'tolower($1) == "location:" { print $2 }')
echo "$loc"   # /v2/images/blob-demo/blobs/uploads/<session>
```

The response also carries `Docker-Upload-UUID: <session>` and `Range: 0-0`; the `Range` is the byte span received so
far, empty at the start. `<session>` lives in memory on this peryx process, so it does not survive a restart.

## Append the first two chunks

Each `PATCH` sends one chunk with a `Content-Range: <start>-<end>` that begins exactly where the last chunk ended.
Append `part-a` at bytes `0-9`, then `part-b` at `10-19`:

```shell
curl -sS -i -u _:demo-secret -X PATCH \
  -H 'Content-Type: application/octet-stream' \
  -H 'Content-Range: 0-9' \
  --data-binary @part-a "http://127.0.0.1:4433$loc"
# 202 Accepted, Range: 0-9

curl -sS -i -u _:demo-secret -X PATCH \
  -H 'Content-Type: application/octet-stream' \
  -H 'Content-Range: 10-19' \
  --data-binary @part-b "http://127.0.0.1:4433$loc"
# 202 Accepted, Range: 0-19
```

Every `202` echoes the updated `Range: 0-<end>`, so `Range: 0-19` means 20 bytes have landed and the next chunk must
start at byte `20`.

## Trigger a 416 and recover

Now send the third chunk with the wrong `Content-Range`, as if you had lost track and skipped ahead to byte `30`. peryx
rejects the gap with `416` and keeps the 20 bytes already staged:

```shell
curl -sS -i -u _:demo-secret -X PATCH \
  -H 'Content-Type: application/octet-stream' \
  -H 'Content-Range: 30-40' \
  --data-binary @part-c "http://127.0.0.1:4433$loc"
# 416 Range Not Satisfiable
# Location: /v2/images/blob-demo/blobs/uploads/<session>
# Docker-Upload-UUID: <session>
# Range: 0-19
```

The `416` carries the session's `Location` and `Docker-Upload-UUID` alongside `Range: 0-19`, so you resume from those
coordinates instead of restarting the whole upload. `Range: 0-19` says byte `20` is the next one peryx expects; resend
the chunk there:

```shell
curl -sS -i -u _:demo-secret -X PATCH \
  -H 'Content-Type: application/octet-stream' \
  -H 'Content-Range: 20-30' \
  --data-binary @part-c "http://127.0.0.1:4433$loc"
# 202 Accepted, Range: 0-30
```

## Finish with a digest check

`PUT` closes the session under the digest you recorded. peryx appends any body on the `PUT` (none here), verifies the
assembled bytes against `<digest>`, and commits the blob:

```shell
curl -sS -i -u _:demo-secret -X PUT \
  "http://127.0.0.1:4433$loc?digest=sha256:$(sha256sum blob.bin | cut -d' ' -f1)"
# 201 Created
# Location: /v2/images/blob-demo/blobs/sha256:<hex>
# Docker-Content-Digest: sha256:<hex>
```

A digest that does not match the uploaded bytes, or a missing `digest` query, is `400 DIGEST_INVALID` and nothing is
committed. Confirm the blob is now served:

```shell
curl -sS -I -u _:demo-secret \
  "http://127.0.0.1:4433/v2/images/blob-demo/blobs/sha256:$(sha256sum blob.bin | cut -d' ' -f1)"
# 200 OK, Content-Length: 31
```

## Cancel instead of finishing

A session you decide to abandon does not have to wait to time out. Open one and `DELETE` it: peryx drops the session and
its staged bytes and answers `204`:

```shell
loc=$(curl -sS -u _:demo-secret -X POST -D - -o /dev/null \
  http://127.0.0.1:4433/v2/images/blob-demo/blobs/uploads/ \
  | tr -d '\r' | awk 'tolower($1) == "location:" { print $2 }')

curl -sS -i -u _:demo-secret -X DELETE "http://127.0.0.1:4433$loc"
# 204 No Content

curl -sS -i -u _:demo-secret -X DELETE "http://127.0.0.1:4433$loc"
# 404 Not Found (BLOB_UPLOAD_UNKNOWN): the session is already gone
```

## Pull a manifest addressed by sha512

The upload side keys everything on sha256, but a manifest peryx *reads* through a proxy may be advertised in another
algorithm. Most registries content-address with sha256, but the
[image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests) allows others,
and a registry may advertise its `Docker-Content-Digest` in sha512. This last part makes that case concrete: you run a
tiny stand-in upstream that serves a manifest under a sha512 digest, proxy it through a cached peryx index, and watch
peryx accept it, where it once returned `502`.

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

Point a cached index at the stub. There is nothing to configure for the digest algorithm; it is the default behavior.
Save this as `sha512.toml` and start a second peryx (stop the hosted one first, or give this one its own port):

```toml
# sha512.toml
[[index]]
name = "reg"
route = "reg"
ecosystem = "oci"
cached = "http://127.0.0.1:5000"
```

```shell
peryx serve --config sha512.toml   # listening on 127.0.0.1:4433
```

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

A client that already holds the upstream's sha512 digest can pull by it directly. peryx serves the bytes under the
digest you asked for and echoes it back:

```shell
curl -si http://127.0.0.1:4433/v2/reg/demo/manifests/sha512:$(printf '%s' '{"schemaVersion":2,"config":{}}' | sha512sum | cut -d' ' -f1)
```

The `docker-content-digest` on that response is the `sha512:` value from the request, while the cache still keys the
bytes on sha256 underneath. peryx verifies the sha256 it computes and trusts the algorithm it cannot recompute, so a
registry that content-addresses with sha512 works through peryx without any special configuration.

## Where next

- [Work with registry behavior](@/ecosystems/oci/guides/registry-behavior.md): the cancel, resume, and non-sha256 moves
  as recipes you reach for when a real push stalls or an upstream uses another algorithm.
- [Registry behavior](@/ecosystems/oci/reference/registry-behavior.md): the exact statuses, headers, and digest rules.
- [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md): every `/v2/` route peryx serves.
