+++
title = "From cloud registries"
description = "ECR, GHCR, Google Artifact Registry, and ACR: what their pull-through and billing cost you, and how peryx fronts or replaces them."
weight = 7
[extra]
logos = [ "logos/gitlab.svg", "logos/googlecloud.svg"]
+++

The hosted container registries integrate with their platform's identity and billing. That is the draw and the tax:
metered storage and egress, tokens that expire mid-pipeline, and pull-rate limits that stall CI. peryx either replaces
them for the images you build or sits in front of them as a caching cached index, so a base image is pulled from the
cloud once and served from local disk after.

What their pull-through and limits look like today:

- **[Amazon ECR](https://aws.amazon.com/ecr/)** offers a
  [pull-through cache](https://docs.aws.amazon.com/AmazonECR/latest/userguide/pull-through-cache.html) for a fixed set
  of upstreams, but the cache is per-repository and its images still bill against
  [ECR storage and data-transfer pricing](https://aws.amazon.com/ecr/pricing/); auth is a 12-hour token from
  `aws ecr get-login-password`, so every runner re-logs in.
- **[GitHub Container Registry (GHCR)](https://docs.github.com/packages)** hosts images but has no pull-through cache of
  [Docker Hub](https://hub.docker.com/), so a build that pulls a public base image still hits Docker Hub and its
  [rate limits](https://docs.docker.com/docker-hub/download-rate-limit/) on every cold runner.
- **[Google Artifact Registry](https://cloud.google.com/artifact-registry)** has remote (pull-through) and virtual
  (aggregation) Docker repositories (the closest cloud analog to peryx's model, split across resource types), with
  [metered storage and egress](https://cloud.google.com/artifact-registry/pricing).
- **[Azure Container Registry](https://learn.microsoft.com/en-us/azure/container-registry/)** caches upstream images
  with [artifact cache](https://learn.microsoft.com/en-us/azure/container-registry/tutorial-artifact-cache), gated
  behind the Standard/Premium tiers and
  [metered per GiB](https://azure.microsoft.com/en-us/pricing/details/container-registry/).

## Why peryx

Self-hosted: no per-GiB meter, no token treadmill, and one config file instead of per-upstream cache-rule resources. A
single content-addressed blob store is shared across every index, so a base layer pulled once serves every image and
every ecosystem. When the images must stay in the cloud registry (platform IAM, compliance), keep it as the push target
and put peryx in front as a cached index: clients get caching and single-flight fetch; the registry keeps ownership.

## The renames

Point a peryx `cached` OCI index at the registry's `/v2/` endpoint; its repository path becomes the index route prefix.

| Registry  | Its `/v2/` host                         | As a peryx cached index                                                        |
| --------- | --------------------------------------- | ------------------------------------------------------------------------------ |
| ECR       | `{acct}.dkr.ecr.{region}.amazonaws.com` | `username = "AWS"` + `password` (the 12-hour `get-login-password` token)       |
| GHCR      | `ghcr.io`                               | `username` + `password` (a personal access token with `read:packages`)         |
| Google AR | `{loc}-docker.pkg.dev`                  | `username = "_json_key_base64"` + `password` (the encoded service-account key) |
| Azure ACR | `{registry}.azurecr.io`                 | `username` + `password` (a token or service principal)                         |

## Pitfalls

- ECR's short-lived tokens make it the one upstream peryx cannot front unattended today; a refresh-command hook is on
  the roadmap.
- Cloud IAM does not translate: peryx reads are open to its network, pushes are token-gated per index.
- Egress from the registry to peryx is still billed by the provider; the cache means you pay it once per layer.
