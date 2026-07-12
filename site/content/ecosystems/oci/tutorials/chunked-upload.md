+++
title = "Push a blob chunk by chunk"
description = "Drive an OCI chunked blob upload by hand with curl: start a session, PATCH chunks with Content-Range, recover from a 416, cancel with DELETE, and finish with PUT."
weight = 5
+++

`docker push` and `crane push` upload a blob for you in one call, so you never see the steps underneath. This tutorial
runs those steps by hand with `curl` against a hosted index, so the chunked-upload state machine the
[distribution spec](https://github.com/opencontainers/distribution-spec) defines becomes something you can watch: you
start a session, append the blob one chunk at a time, deliberately send a chunk out of order to trigger a `416` and
recover from it, cancel a session, and finish a real upload with a digest check. It takes about ten minutes and builds
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

## Where next

- [Cancel and resume an OCI push](@/ecosystems/oci/guides/cancel-and-resume-push.md): the same two moves as a recipe you
  reach for when a real push stalls.
- [Upload sessions and referrers validation](@/ecosystems/oci/reference/upload-sessions.md): the exact statuses,
  headers, and digest rules.
- [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md): every `/v2/` route peryx serves.
