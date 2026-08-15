+++
title = "From cloud registries"
description = "ECR, GHCR, Google Artifact Registry, and ACR: what their pull-through and billing cost you, and how peryx fronts or replaces them."
weight = 7
[extra]
logos = [ "logos/gitlab.svg", "logos/googlecloud.svg"]
+++

Hosted registries integrate with their platform's identity and billing. They charge for storage and egress, use expiring
tokens, and may enforce pull-rate limits. peryx can host images or cache a cloud registry so each base layer crosses the
upstream boundary once.

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

## Cost and protocol differences

A self-hosted peryx instance has no per-GiB service charge and uses one configuration file. Its content-addressed blob
store spans indexes, so one fetched base layer can serve multiple images. If platform IAM or compliance requires the
cloud registry to remain the push target, use peryx as a cached index in front of it.

## Configuration mapping

Point a peryx `cached` OCI index at the registry's `/v2/` endpoint; its repository path becomes the index route prefix.

| Registry                 | `/v2/` host                             | Cached-index credentials                                                          |
| ------------------------ | --------------------------------------- | --------------------------------------------------------------------------------- |
| ECR                      | `{acct}.dkr.ecr.{region}.amazonaws.com` | `username = "AWS"` and the 12-hour `get-login-password` token as `password`       |
| GHCR                     | `ghcr.io`                               | `username` and a personal access token with `read:packages` as `password`         |
| Google Artifact Registry | `{loc}-docker.pkg.dev`                  | `username = "_json_key_base64"` and the encoded service-account key as `password` |
| Azure ACR                | `{registry}.azurecr.io`                 | `username` and a token or service-principal secret as `password`                  |

## Constraints

- ECR's short-lived tokens make it the one upstream peryx cannot front unattended today; a refresh-command hook is on
  the roadmap.
- Cloud IAM does not translate: peryx reads are open to its network, pushes are token-gated per index.
- Egress from the registry to peryx is still billed by the provider; the cache means you pay it once per layer.
