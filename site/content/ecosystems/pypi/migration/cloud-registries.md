+++
title = "From cloud registries"
description = "CodeArtifact, GitLab, Azure Artifacts, and Google Artifact Registry: what their Python support misses, and how peryx fronts or replaces them."
weight = 7
[extra]
logos = [ "logos/gitlab.svg", "logos/googlecloud.svg"]
+++

The hosted registries integrate with their platform's identity and billing. That is the draw and the tax: metered
storage and egress, tokens that expire mid-pipeline, and Python protocol support that lags years behind pypi.org. peryx
either replaces them for Python or sits in front of them as a caching, protocol-upgrading cached index.

What their own documentation states today:

- **AWS CodeArtifact** [documents](https://docs.aws.amazon.com/codeartifact/latest/ug/python-compatibility.html) that it
  "does not support PyPI's XML-RPC or JSON APIs" and serves only per-project simple pages; auth tokens
  [expire after at most 12 hours](https://docs.aws.amazon.com/codeartifact/latest/ug/tokens-authentication.html), so
  pipelines rerun a login step; storage, requests, and egress are
  [metered](https://aws.amazon.com/codeartifact/pricing/).
- **GitLab's PyPI registry**
  [implements only the PEP 503 HTML API](https://gitlab.com/gitlab-org/gitlab/-/work_items/586978) and *forwards* misses
  to pypi.org rather than caching them; its caching virtual registry
  [does not cover PyPI yet](https://gitlab.com/groups/gitlab-org/-/epics/3612).
- **Azure Artifacts** saves upstream packages into the feed on first fetch (good), but
  [custom upstreams exist for npm only](https://learn.microsoft.com/en-us/azure/devops/artifacts/concepts/upstream-sources?view=azure-devops)
  (Python gets pypi.org and other Azure feeds), storage is
  [metered per GiB](https://azure.microsoft.com/en-us/pricing/details/devops/azure-devops-services/), and its lazy
  upstream indexing [confuses uv](https://github.com/astral-sh/uv/issues/11440).
- **Google Artifact Registry** has remote (pull-through) and virtual (aggregation) Python repositories (the closest
  cloud analog to peryx's model, split across three resource types) with
  [PEP 658 still an open feature request](https://issuetracker.google.com/issues/300035693) and
  [metered storage](https://cloud.google.com/artifact-registry/pricing).

## Peryx differences

Self-hosted: no per-GiB meter, no token treadmill, PEP 691/658/700 served by default, and one config file instead of
domain/repository/upstream resource graphs. When the packages must stay in the cloud registry (platform IAM,
compliance), keep it as the upload target and put peryx in front as a cached index: clients get caching and modern
protocols; the registry keeps ownership.

## Configuration mapping

| Registry        | Its simple URL                                                                      | As a peryx cached index                                                        |
| --------------- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| CodeArtifact    | `https://{domain}-{acct}.d.codeartifact.{region}.amazonaws.com/pypi/{repo}/simple/` | use an upstream credential helper to refresh its short-lived token             |
| GitLab          | `https://host/api/v4/projects/{id}/packages/pypi/simple`                            | `username` + `password` (a personal or deploy token)                           |
| Azure Artifacts | `https://pkgs.dev.azure.com/{org}/{proj}/_packaging/{feed}/pypi/simple/`            | `username` (any) + `password` (a PAT)                                          |
| Google AR       | `https://{loc}-python.pkg.dev/{proj}/{repo}/simple/`                                | `username = "_json_key_base64"` + `password` (the encoded service-account key) |

## Pitfalls

- CodeArtifact needs `credential_exec` or another configured credential source that returns a fresh token before expiry.
- Cloud IAM does not translate. Configure peryx access tokens and `anonymous_read` for client access.
- Egress from the registry to peryx is still billed by the provider; the cache means you pay it once per artifact.
