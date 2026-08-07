+++
title = "Store Helm charts, SBOMs, and signatures"
description = "Use a hosted OCI index to push Helm charts and arbitrary artifacts, then attach and discover SBOMs and signatures through the referrers API."
weight = 7
+++

An OCI registry provides content-addressed storage for container images and other artifacts. Helm charts, WASM modules,
config bundles, SBOMs, and signatures use OCI manifests with media types that peryx serves. A
[hosted index](@/core/glossary.md#roles) can hold images and their supply-chain metadata; the referrers API attaches a
signature or SBOM to the image digest it describes. First
[run a container registry](@/ecosystems/oci/guides/container-registry.md) to configure the hosted index and the HTTP or
HTTPS transport used below.

## Configure a hosted index

Referrers and artifact pushes need a writable index. A [cached](@/core/glossary.md#roles) proxy is read-only, so declare
a `hosted` index with a write-granting `[[index.access_token]]`:

```toml
# peryx.toml
host = "127.0.0.1"
port = 4433

[[index]]
name = "artifacts"
route = "artifacts"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "upload"
secret = "<token>"
actions = ["write", "delete"]
```

Run it with `peryx serve --config peryx.toml`. peryx accepts any username and treats the access token's secret as the
Basic-auth password. The `--plain-http` flags below are what each client needs to talk HTTP to a
[loopback](@/ecosystems/oci/guides/local-transport.md) registry; over the network give peryx a certificate
([serve HTTPS](@/core/serve-https.md)) and drop them.

## Push and pull a Helm chart

Helm speaks OCI. Log in once, then push a packaged chart to the index route; Helm appends the chart name, so
`mychart-1.0.0.tgz` lands at `artifacts/mychart`:

```shell
helm registry login 127.0.0.1:4433 -u _ -p <token> --plain-http
helm push mychart-1.0.0.tgz oci://127.0.0.1:4433/artifacts --plain-http
helm pull oci://127.0.0.1:4433/artifacts/mychart --version 1.0.0 --plain-http
```

peryx stores the chart's config and layer blobs in its content-addressed store and serves the manifest by tag or digest.
Helm uses the conformant registry as a backend.

## Push and pull an arbitrary artifact

For files that are not images or charts, [oras](https://oras.land/) packs any set of files into a manifest. Set an
`--artifact-type` so consumers can tell what the artifact is:

```shell
oras login 127.0.0.1:4433 -u _ -p <token> --plain-http
oras push --plain-http 127.0.0.1:4433/artifacts/config-bundle:1.0 \
  --artifact-type application/vnd.acme.config.v1+json config.yaml
oras pull --plain-http 127.0.0.1:4433/artifacts/config-bundle:1.0
```

## Attach an SBOM or signature

The referrers API links one manifest to another. peryx records the descriptor of a pushed manifest that declares a
`subject` under that subject digest and echoes an `OCI-Subject` header on the `201`. Point `oras attach` at the artifact
and pass it the file to attach.

```shell
oras attach --plain-http --artifact-type application/spdx+json \
  127.0.0.1:4433/artifacts/my-app:1.0 sbom.spdx.json
```

The subject uses the target manifest's digest, so the link survives a later retag of `my-app:1.0`. Discover attached
artifacts with `oras discover`, which reads `GET /v2/artifacts/my-app/referrers/<digest>`:

```shell
oras discover --plain-http 127.0.0.1:4433/artifacts/my-app:1.0
oras discover --plain-http --artifact-type application/spdx+json 127.0.0.1:4433/artifacts/my-app:1.0
```

The second call filters on the server with `?artifactType=application/spdx+json`; peryx returns only the matching
descriptors and sets `OCI-Filters-Applied: artifactType` so the client knows the filter took effect. The same mechanism
records any tool's referrers: a cosign signature run in its OCI 1.1 referrers mode writes a manifest with a `subject`,
and peryx indexes it as it indexes the `oras attach` manifest above.

## Discover referrers when proxying

A [cached](@/core/glossary.md#roles) index that fronts an upstream also serves referrers, and it bridges the gap for a
registry that predates the API. Such a registry answers `404` on the `/referrers/` route and instead publishes a
subject's referrers under a fallback tag derived from the subject digest, `sha256:<hex>` written as `sha256-<hex>`. On
that `404` peryx fetches the fallback tag and merges its entries, so a signature or SBOM pushed before the API existed
stays discoverable through the cache. If the upstream serves the referrers API, peryx uses its response without asking
for the tag.

## Pitfalls

peryx rejects a manifest push up front with `MANIFEST_BLOB_UNKNOWN` when its config or layer blobs are not in the store
yet, because a resolver would otherwise `404` on the missing piece after the push reported success. `helm push`,
`oras push`, and `oras attach` upload blobs before the manifest, so they satisfy this on their own; a hand-rolled push
must send the blobs first.

The immutable subject digest keys referrers. Attaching to a moving tag records the link against the digest digest the
tag resolved to at push time, so re-attach after you push a new digest to the same tag.

A well-formed subject digest with no references returns `200` with an empty `manifests` list. A malformed digest returns
`400 DIGEST_INVALID`. An empty list means "no referrers".

## Related

- Set up the hosted index and the transport rules these commands assume:
  [run a container registry](@/ecosystems/oci/guides/container-registry.md)
- Gate writes and hand out scoped pull and push tokens:
  [make an OCI index private and issue tokens](@/ecosystems/oci/guides/token-auth.md)
- The exact referrers response, filter header, and digest validation:
  [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md#referrers) and
  [registry behavior](@/ecosystems/oci/reference/registry-behavior.md#referrers-subject-digest-validation)
- Serve trusted HTTPS so clients need no `--plain-http`: [serve HTTPS](@/core/serve-https.md)
