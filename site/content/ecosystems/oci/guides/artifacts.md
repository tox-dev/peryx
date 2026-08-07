+++
title = "Store Helm charts, SBOMs, and signatures"
description = "Use a hosted OCI index for more than container images: push Helm charts and arbitrary artifacts, then attach and discover SBOMs and signatures through the referrers API."
weight = 7
+++

An OCI registry is content-addressed storage for any artifact, not only container images. A Helm chart, a WASM module, a
config bundle, an SBOM, and a signature are all OCI manifests with a media type peryx already serves. So the same
[hosted index](@/core/glossary.md#roles) that holds your images can hold their supply-chain metadata, and the referrers
API keeps a signature or SBOM attached to the exact image digest it describes. This guide pushes a chart and an
arbitrary artifact, then attaches and discovers referrers. It builds on
[run a container registry](@/ecosystems/oci/guides/container-registry.md), which sets up the hosted index and explains
the plain-HTTP-versus-HTTPS transport rule the commands below rely on.

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

Helm speaks OCI natively. Log in once, then push a packaged chart to the index route; Helm appends the chart name, so
`mychart-1.0.0.tgz` lands at `artifacts/mychart`:

```shell
helm registry login 127.0.0.1:4433 -u _ -p <token> --plain-http
helm push mychart-1.0.0.tgz oci://127.0.0.1:4433/artifacts --plain-http
helm pull oci://127.0.0.1:4433/artifacts/mychart --version 1.0.0 --plain-http
```

peryx stores the chart's config and layer blobs in its content-addressed store and serves the manifest back by tag or
digest like any image. Nothing about the index is Helm-specific; it is a conformant registry that Helm treats as a
backend.

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

The referrers API links one manifest to another. When a pushed manifest declares a `subject`, peryx records its
descriptor under that subject digest and echoes an `OCI-Subject` header on the `201`. `oras attach` builds that manifest
for you: point it at the artifact you are describing and hand it the file to attach.

```shell
oras attach --plain-http --artifact-type application/spdx+json \
  127.0.0.1:4433/artifacts/my-app:1.0 sbom.spdx.json
```

The subject is the target's manifest digest, not its tag, so the link survives a later retag of `my-app:1.0`. Discover
what is attached with `oras discover`, which reads `GET /v2/artifacts/my-app/referrers/<digest>`:

```shell
oras discover --plain-http 127.0.0.1:4433/artifacts/my-app:1.0
oras discover --plain-http --artifact-type application/spdx+json 127.0.0.1:4433/artifacts/my-app:1.0
```

The second call filters on the server with `?artifactType=application/spdx+json`; peryx returns only the matching
descriptors and sets `OCI-Filters-Applied: artifactType` so the client knows the filter took effect. The same mechanism
records any tool's referrers: a cosign signature run in its OCI 1.1 referrers mode writes a manifest with a `subject`,
and peryx indexes it the same as the `oras attach` above.

## Discover referrers when proxying

A [cached](@/core/glossary.md#roles) index that fronts an upstream also serves referrers, and it bridges the gap for a
registry that predates the API. Such a registry answers `404` on the `/referrers/` route and instead publishes a
subject's referrers under a fallback tag derived from the subject digest, `sha256:<hex>` written as `sha256-<hex>`. On
that `404` peryx fetches the fallback tag and merges its entries, so a signature or SBOM pushed before the API existed
stays discoverable through the cache. When the upstream serves the referrers API itself, peryx uses its response and
never asks for the tag.

## Pitfalls

peryx rejects a manifest push up front with `MANIFEST_BLOB_UNKNOWN` when its config or layer blobs are not in the store
yet, because a resolver would otherwise `404` on the missing piece after the push reported success. `helm push`,
`oras push`, and `oras attach` upload blobs before the manifest, so they satisfy this on their own; a hand-rolled push
must send the blobs first.

Referrers are keyed by the immutable subject digest. Attaching to a moving tag still records the link against whatever
digest the tag resolved to at push time, so re-attach after you push a new digest to the same tag.

A well-formed subject digest that nothing references is a `200` with an empty `manifests` list, not a `404`; only a
malformed digest is an error (`400 DIGEST_INVALID`). Treat the empty list as "no referrers", not as a failure.

## Related

- Set up the hosted index and the transport rules these commands assume:
  [run a container registry](@/ecosystems/oci/guides/container-registry.md)
- Gate writes and hand out scoped pull and push tokens:
  [make an OCI index private and issue tokens](@/ecosystems/oci/guides/token-auth.md)
- The exact referrers response, filter header, and digest validation:
  [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md#referrers) and
  [registry behavior](@/ecosystems/oci/reference/registry-behavior.md#referrers-subject-digest-validation)
- Serve trusted HTTPS so clients need no `--plain-http`: [serve HTTPS](@/core/serve-https.md)
