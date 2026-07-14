+++
title = "HTTP endpoints"
description = "The OCI distribution-spec /v2/ routes peryx serves: manifests, blobs, uploads, tags, and referrers."
weight = 2
+++

peryx serves the [OCI distribution spec](https://github.com/opencontainers/distribution-spec) `/v2/` pull-and-push API.
Most routes are `/v2/<name>/…`, where `<name>` carries the index route as a prefix: peryx matches the longest configured
OCI index route that segment-aligns with `<name>`, and the remainder is the upstream repository. An index at route
`dockerhub` serves [Docker Hub](https://hub.docker.com/)'s `library/alpine` as `/v2/dockerhub/library/alpine/…`. A
request whose `<name>` matches no OCI index route answers `404 NAME_UNKNOWN`. The version check `/v2/`, the token
endpoint `/v2/token`, and the repository catalog `/v2/_catalog` are the routes not scoped to a `<name>`. For the concept
map, see [OCI](@/ecosystems/oci/_index.md); for the wire standards, see
[standards](@/ecosystems/oci/reference/standards.md).

`<name>` is one or more lowercase path components (`[a-z0-9._-]`, no bare `.`/`..`, ≤ 255 chars). A manifest
`<reference>` is a tag (`[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`) or a digest (`algorithm:encoded`). Blob digests must be
`sha256:…`; any other algorithm is `400 DIGEST_INVALID`.

## Endpoints

| Method       | Path                                 | Purpose                                | Success       |
| ------------ | ------------------------------------ | -------------------------------------- | ------------- |
| `GET`        | `/v2/`                               | API version check                      | `200` / `401` |
| `GET`        | `/v2/token`                          | Mint a scoped Bearer token             | `200`         |
| `GET`        | `/v2/_catalog`                       | List every repository, paginated       | `200`         |
| `GET` `HEAD` | `/v2/<name>/manifests/<reference>`   | Pull a manifest by tag or digest       | `200`         |
| `PUT`        | `/v2/<name>/manifests/<reference>`   | Push a manifest                        | `201`         |
| `DELETE`     | `/v2/<name>/manifests/<reference>`   | Delete a manifest or untag             | `202`         |
| `GET` `HEAD` | `/v2/<name>/blobs/<digest>`          | Pull a blob (range-capable)            | `200` / `206` |
| `DELETE`     | `/v2/<name>/blobs/<digest>`          | Delete a blob                          | `202`         |
| `GET`        | `/v2/<name>/blobs/<digest>/contents` | List a layer's files, or preview one   | `200`         |
| `POST`       | `/v2/<name>/blobs/uploads/`          | Begin, mount, or monolithically push   | `202` / `201` |
| `GET`        | `/v2/<name>/blobs/uploads/<session>` | Report upload progress                 | `204`         |
| `PATCH`      | `/v2/<name>/blobs/uploads/<session>` | Append a chunk                         | `202`         |
| `PUT`        | `/v2/<name>/blobs/uploads/<session>` | Finish an upload                       | `201`         |
| `DELETE`     | `/v2/<name>/blobs/uploads/<session>` | Cancel an upload session               | `204`         |
| `GET`        | `/v2/<name>/tags/list`               | List tags, paginated                   | `200`         |
| `GET`        | `/v2/<name>/referrers/<digest>`      | List manifests referring to `<digest>` | `200`         |

## Version check

`GET /v2/` (with or without the trailing slash) is the first request every container client sends. It answers `200` with
`Docker-Distribution-API-Version: registry/2.0` and an empty body when no OCI index restricts access, or when the
request carries a credential the realm accepts. When an OCI index restricts access and the request carries none, it
answers `401` with `WWW-Authenticate: Bearer realm="<base>/v2/token",service="peryx"`, the challenge that starts
`docker login`. The `/v2/token` endpoint, the scope grammar, and the resource-route error codes are covered in
[token authentication](@/ecosystems/oci/reference/token-auth.md).

## Manifests

peryx stores a manifest byte-for-byte and addresses it by the sha256 of those exact bytes, so the
`Docker-Content-Digest` a client verifies always matches what it pushed or pulled.

`GET`/`HEAD /v2/<name>/manifests/<reference>` resolves the reference through the index's members hosted-first (a hosted
image shadows the same name upstream, the [dependency-confusion defense](@/core/glossary.md#shadowing)). A hosted member
reads its stored tag mapping; an online proxy member revalidates the tag against upstream and caches the result. A pull
by digest is scoped to the requesting repository: peryx serves it from the content-addressed store only when a member
records that digest under this repository, meaning a manifest pushed or tagged here, a child of an image index or
manifest list it serves, or a referrer pushed here. A proxy member still pulls an unauthorized miss through its upstream
under this repository, so a legitimate pull-through stays intact, but a digest that no member records and no proxy can
fetch is `404 MANIFEST_UNKNOWN`, even when the same bytes sit in the store under another repository. See
[how peryx scopes and serves manifest reads](@/ecosystems/oci/manifest-serving.md) for the reasoning. The response
carries the stored `Content-Type`, `Docker-Content-Digest`, and `Content-Length`; a `HEAD` returns those headers with an
empty body. A reference no member can serve is `404 MANIFEST_UNKNOWN`.

When a resolved manifest is an image index or manifest list and the request's `Accept` names neither list media type,
peryx serves the index's `linux/amd64` child image manifest instead, the substitution that lets legacy Docker (below
17.06) and older tooling that send only the schema-2 image type still pull. The response then carries the child's
`Content-Type` and `Docker-Content-Digest`, reading it from the store or fetching it by digest through a proxy member; a
`HEAD` returns the same headers with an empty body. An `Accept` that is absent or names a list type, a manifest that is
not a list, and a list without a `linux/amd64` child all serve the resolved manifest unchanged. Modern docker, podman,
containerd, and oras send full `Accept` lists that name the index types, so they receive the index.

`PUT /v2/<name>/manifests/<reference>` stores the request body under its canonical `sha256:` digest and, when
`<reference>` is a tag, points that tag at the digest. The `Content-Type` header is recorded as the manifest's media
type (defaulting to `application/vnd.oci.image.manifest.v1+json`); peryx ignores any `Content-Type` parameters, so
`application/vnd.oci.image.manifest.v1+json; charset=utf-8` matches and stores as the bare base type rather than failing
with `400 MANIFEST_INVALID`. A media type peryx does not accept as a manifest is `400 MANIFEST_INVALID`. A body over 4
MiB produces `413 Payload Too Large`, distinct from the `502` a broken transfer returns. For a digest reference, peryx
returns `400 DIGEST_INVALID` unless the body hashes to that digest. peryx returns `400 MANIFEST_BLOB_UNKNOWN` when the
manifest names a config or layer that this repository cannot serve. A missing child manifest produces the same error. On
success, peryx returns `201` with `Location` and `Docker-Content-Digest`. When the manifest declares a `subject`, peryx
sends its digest in `OCI-Subject` and records it for the referrers API.

`DELETE /v2/<name>/manifests/<reference>` removes the manifest by digest, or drops the tag mapping when `<reference>` is
a tag. Success is `202`; a reference that was not present is `404 MANIFEST_UNKNOWN`.

## Blobs

peryx deduplicates content-addressed blob bytes across indexes. A separate repository link controls access, so knowledge
of a digest under another repository does not make it readable under `<name>`.

peryx serves `GET` and `HEAD /v2/<name>/blobs/<digest>` from the store. Without a cached repository link, peryx pulls
through an online proxy; concurrent misses for one digest share one upstream fetch. If the cache contains bytes through
another repository, peryx sends `HEAD` to this repository's upstream before it adds the link, avoiding a second body
download. peryx sends `Content-Type: application/octet-stream` and `Accept-Ranges: bytes`, plus the digest and length
headers. A `Range: bytes=…` request produces `206` with `Content-Range`. peryx returns `416` with
`Content-Range: bytes */<size>` for an unsatisfiable or malformed `bytes` range, and it ignores other range units. A
missing digest produces `404 BLOB_UNKNOWN`; a non-`sha256` digest produces `400 DIGEST_INVALID`.

peryx removes this repository's link for `DELETE /v2/<name>/blobs/<digest>` and returns `202`. A missing link produces
`404 BLOB_UNKNOWN`. peryx leaves the payload in the shared content store. `cache purge orphaned-blobs` removes it after
each installed ecosystem driver reports no reference.

### Layer contents

`GET /v2/<name>/blobs/<digest>/contents` is peryx's own layer browser, not a distribution-spec route (a plain registry
answers `404` here, so it never collides with a pull). It ensures the layer blob is present (fetching it once through
the single-flight gate on a miss), then reads it as a tar. Without a query it answers `200` with
`{"members": [{"path", "size", "kind", "previewable"}, …]}`, listing the layer's files. With `?member=<path>&offset=<n>`
it previews one text member: `text/plain` bytes plus `x-peryx-member-size`, `x-peryx-member-offset`, and (when more
follows) `x-peryx-next-offset` headers, so a large member pages in bounded chunks. A binary member is `415`, an unknown
member `404`, an offset past the member `416`, and an unreadable layer `422`. The web UI's file browser reads this route
to show a layer's contents.

### Uploads

A push writes blobs through an upload session started with `POST /v2/<name>/blobs/uploads/`. Three shapes:

- **Cross-repo mount**: `POST …/uploads/?mount=<digest>&from=<source-name>`. Use the full repository name, including its
  peryx index route. If the source links `<digest>` and stores its bytes, peryx checks pull permission before it links
  the target. peryx returns `201` with the blob location and digest headers. If the source lacks the link or bytes,
  peryx opens a `202` upload session. peryx takes the same path without `from`; missing pull permission produces the
  source's `401` challenge.
- **Monolithic**: `POST …/uploads/?digest=<digest>` with the blob as the body. peryx streams it in, verifies the digest
  on commit, and answers `201`.
- **Chunked**: a bare `POST …/uploads/` opens a session and answers `202` with
  `Location: /v2/<name>/blobs/uploads/<session>`, `Docker-Upload-UUID`, and `Range: 0-<n>`. The client appends with
  `PATCH` requests, then finishes with `PUT …/uploads/<session>?digest=<digest>`.

`PATCH /v2/<name>/blobs/uploads/<session>` appends a chunk and answers `202` with the updated `Range` and
`Docker-Upload-UUID`. A chunk whose `Content-Range` does not begin where the last one ended (or cannot be parsed) is
`416 Range Not Satisfiable`; the session keeps its bytes, and the response carries `Location`, `Docker-Upload-UUID`, and
`Range: 0-<n>` so the client can resume from those coordinates rather than restart.

`DELETE /v2/<name>/blobs/uploads/<session>` cancels an open session (spec end-14), dropping it and its staged temp file
and answering `204`. An unknown session (including one already committed or cancelled) is `404 BLOB_UPLOAD_UNKNOWN`.

`PUT /v2/<name>/blobs/uploads/<session>?digest=<digest>` appends any trailing body, then verifies and commits under
`<digest>`, answering `201` with `Location` and `Docker-Content-Digest`. A digest mismatch on commit is
`400 DIGEST_INVALID`; a missing `digest` query is also `400 DIGEST_INVALID`.

`GET /v2/<name>/blobs/uploads/<session>` reports progress: `204` with `Location`, `Docker-Upload-UUID`, and
`Range: 0-<n>`. An unknown session (including one already committed) is `404 BLOB_UPLOAD_UNKNOWN`. Sessions are
in-memory and process-local; they do not survive a restart.

## Tags

`GET /v2/<name>/tags/list` answers `200` with `application/json` `{"name": "<name>", "tags": [...]}`. A lone online
proxy index passes the upstream response through verbatim, forwarding the client's query. Every other case (a hosted
index or a virtual index) unions its members' tags under the requested name, sorted, then applies pagination:
`?n=<count>` caps the page and `?last=<tag>` resumes after a tag. When `n` truncates the set, the response adds a
`Link: </v2/<name>/tags/list?n=<n>&last=<marker>>; rel="next"` header pointing at the next page.

## Catalog

`GET /v2/_catalog` answers `200` with `application/json` `{"repositories": [...]}`, the union of every OCI index's
repositories as clients address them: each entry is the index route joined to the upstream repository, so the names a
`crane catalog` lists are the same ones a client pulls. The set is sorted, then paginated like `tags/list`: `?n=<count>`
caps the page and `?last=<repo>` resumes after a repository, and a truncated page adds a
`Link: </v2/_catalog?n=<n>&last=<marker>>; rel="next"` header. A serve-policy rule omits the repositories it blocks.
peryx requires a Bearer `registry:catalog:*` grant when the token realm runs and an OCI index is private. It puts that
scope in a missing token's `401` challenge and returns `401 insufficient_scope` for a repository token. Without a token
signing, peryx accepts Basic authentication for the private catalog.

## Referrers

`GET /v2/<name>/referrers/<digest>` returns an OCI image index (`application/vnd.oci.image.index.v1+json`) whose
`manifests` are the descriptors of every pushed manifest that declared `<digest>` as its `subject`, aggregated across
the index's members. Each descriptor carries `mediaType`, `digest`, `size`, and (when the source manifest had them)
`artifactType` and `annotations`. A `<digest>` that is not a syntactically valid content digest is `400 DIGEST_INVALID`;
the registered `sha256`/`sha512` algorithms have their fixed hex length enforced, while an unregistered algorithm is
held only to the general grammar. A well-formed but unknown subject is `200` with an empty `manifests`
([digest validation reference](@/ecosystems/oci/reference/registry-behavior.md#referrers-subject-digest-validation)). A
`?artifactType=<type>` query filters the result to the descriptors whose `artifactType` matches, and the response then
carries `OCI-Filters-Applied: artifactType` so a client knows the filter was honored.

## Discovery

`GET /+api` is peryx's cross-ecosystem discovery document, not a `/v2/` route. It lists every configured index; an OCI
index's entry carries its `/v2/` registry URL, the capabilities peryx serves for it, and a `docker pull` snippet (plus
`docker login`/`docker push` when the index accepts writes) with the host taken from the request. `GET /<route>/+api`
returns the single index's entry. The web UI reads the same data to show a copyable pull command on each tag.

## Authentication

Pull requests (the version check and every `GET`/`HEAD` on manifests, blobs, tags, and referrers) take no authentication
when no OCI index restricts access. When an index sets `anonymous_read = false` or configures tokens, peryx challenges
its own pull callers too: the version check and every read route answer `401` with `WWW-Authenticate: Bearer` pointing
at `/v2/token`, the restricted-access handshake the [version check](#version-check) describes and
[token authentication](@/ecosystems/oci/reference/token-auth.md) covers in full. Separately, on the pull-through path
peryx runs the same `401` + `WWW-Authenticate: Bearer` handshake as a *client* against an upstream registry that demands
it, fetching a bearer token from the challenge realm and caching it per scope.

Writes (`PUT`/`DELETE` on manifests, `DELETE` on blobs, every blob upload verb, and the upload-status `GET`) require
`Authorization: Basic` where the password is the target hosted index's `upload_token`; the username is ignored. A
virtual index routes the write to its configured upload-target member. Responses:

- `401 UNAUTHORIZED` with `WWW-Authenticate: Basic realm="peryx"`: missing or wrong credentials.
- `403 DENIED`: the resolved index is read-only (proxy, or virtual with no upload target), or its `upload_token` is
  unset (uploads disabled).
- `404 NAME_UNKNOWN`: `<name>` matches no OCI index route.

`docker login` / `podman login` / `crane auth login` against peryx use Basic auth with the token as the password.

## Error responses

Errors use the distribution-spec shape `{"errors": [{"code": "<CODE>", "message": "..."}]}` with
`Content-Type: application/json`, each code paired with its canonical status:

| Code                    | Status | Meaning                                                                        |
| ----------------------- | ------ | ------------------------------------------------------------------------------ |
| `NAME_UNKNOWN`          | `404`  | `<name>` matches no OCI index route                                            |
| `MANIFEST_UNKNOWN`      | `404`  | No member can serve the reference                                              |
| `BLOB_UNKNOWN`          | `404`  | The blob is neither stored nor upstream                                        |
| `BLOB_UPLOAD_UNKNOWN`   | `404`  | No such upload session                                                         |
| `DIGEST_INVALID`        | `400`  | Non-`sha256` digest, bytes that do not match, or a malformed referrers digest  |
| `MANIFEST_BLOB_UNKNOWN` | `400`  | A pushed manifest references a blob or child manifest not in the store         |
| `MANIFEST_INVALID`      | `400`  | An unsupported manifest media type on push, or an upstream digest disagreement |
| `DENIED`                | `403`  | Read-only index, or uploads disabled                                           |
| `TOOMANYREQUESTS`       | `429`  | An upstream rate-limited a pull-through (carries `Retry-After`)                |
| `UNAUTHORIZED`          | `401`  | Missing or wrong upload credentials                                            |
| `UNSUPPORTED`           | `405`  | The method is not defined for that route                                       |

An upstream that fails or answers unexpectedly during a pull-through returns `502` with code `UNKNOWN`, so the puller
does not mistake a gateway fault for a client error it would not retry. An upstream that rate-limits the pull-through
instead returns `429` with code `TOOMANYREQUESTS`, forwarding the upstream's `Retry-After` so the client backs off
rather than hammering.

A manifest push whose body exceeds the 4 MiB cap answers `413 Payload Too Large` with code `SIZE_INVALID`. The
distribution spec defines no size-specific code, so peryx reuses `SIZE_INVALID` under the overridden status rather than
add one, and reserves `502` for a genuine transport fault while reading the body.
