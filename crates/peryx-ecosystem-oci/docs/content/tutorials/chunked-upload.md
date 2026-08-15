+++
title = "Push a blob chunk by chunk"
description = "Use curl to upload chunks, recover from 416, cancel sessions, commit blobs, and proxy sha512 manifests."
weight = 5
aliases = [ "/ecosystems/oci/tutorials/non-sha256-digest/"]
+++

Use `curl` to exercise the [distribution-spec](https://github.com/opencontainers/distribution-spec) chunked-upload state
machine against a hosted index. The sequence starts, appends, resumes, cancels, and commits upload sessions. It also
proxies a manifest advertised under sha512. Complete [getting started](getting-started.md) first; allow about twenty
minutes.

## Configure a hosted index

An upload session belongs to a hosted index, and writing to it needs a write-granting `[[index.access_token]]` on that
index. Save this as `peryx.toml`:

```toml
# peryx.toml
[[index]]
name = "images"
route = "images"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "upload"
secret = "demo-secret"
actions = ["write", "delete"]
```

Start peryx and leave it running; use a second terminal for the rest:

```shell
peryx serve --config peryx.toml   # listening on 127.0.0.1:4433
```

The upload requests send `-u _:demo-secret`. peryx ignores the username and uses the token as the Basic-auth password.

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

Read `Docker-Upload-UUID: <session>` and `Range: 0-0` from the response. `Range` shows the byte span received so far and
starts empty. peryx creates an opaque random id and records the complete `images/blob-demo` repository name. Keep the
returned `Location` unchanged. A restart discards this process-local state. After one hour without a status check or
`PATCH` attempt, peryx removes the session during the next process-wide maintenance pass. The pass runs once per minute,
so the staged file can remain for up to one minute beyond that deadline.

If the hosted index sets `max_file_size_bytes`, the first `PATCH` or final `PUT` that would cross the limit returns
`403 DENIED` and removes the session. Start a new session with a smaller blob; the rejected bytes never reach the staged
file.

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

The `416` carries the session's `Location` and `Docker-Upload-UUID` alongside `Range: 0-19`, so you can resume without
restarting the upload. `Range: 0-19` says byte `20` is the next one peryx expects; resend the chunk there:

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
[image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md) allows others, and a
registry may advertise its `Docker-Content-Digest` in sha512. Run a small stand-in upstream that serves a manifest under
a sha512 digest, then proxy it through a cached peryx index. peryx accepts the manifest instead of returning the former
`502`.

A local stub makes the sha512 path reproducible. It answers the `/v2/` version check and serves one manifest while
advertising its sha512 digest in `Docker-Content-Digest`. Save it as `upstream.py`:

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

Point a cached index at the stub. Digest-algorithm handling needs no configuration. Save this as `sha512.toml` and start
a second peryx (stop the hosted one first, or give this one its own port):

```toml
# sha512.toml
[[index]]
name = "reg"
route = "reg"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "http://127.0.0.1:5000"
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

Pin the image with that sha256; clients verify the bytes against it. Earlier versions compared the sha512 header with
the computed sha256 and returned `502` without caching the manifest.

A client that already holds the upstream's sha512 digest can pull by it directly. peryx serves the bytes under the
digest you asked for and echoes it back:

```shell
curl -si http://127.0.0.1:4433/v2/reg/demo/manifests/sha512:$(printf '%s' '{"schemaVersion":2,"config":{}}' | sha512sum | cut -d' ' -f1)
```

The `docker-content-digest` on that response is the `sha512:` value from the request, while the cache still keys the
bytes on sha256 underneath. peryx verifies the sha256 it computes and trusts the algorithm it cannot recompute, so a
registry that content-addresses with sha512 works through peryx without any special configuration.

## Next steps

- [Work with registry behavior](../guides/registry-behavior.md): cancellation, resume, and non-sha256 proxy recipes.
- [Registry behavior](../reference/registry-behavior.md): the exact statuses, headers, and digest rules.
- [HTTP endpoints](../reference/endpoints.md): the `/v2/` routes peryx serves.
