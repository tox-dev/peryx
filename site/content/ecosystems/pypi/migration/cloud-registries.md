+++
title = "From cloud registries"
description = "Map CodeArtifact, GitLab, Azure Artifacts, and Google Artifact Registry repositories and credentials to peryx."
weight = 7
[extra]
logos = [ "logos/gitlab.svg", "logos/googlecloud.svg"]
+++

Hosted registries integrate package access with their cloud or development platform. peryx can replace their Python
repositories or cache an existing repository when platform identity, policy, or ownership must remain there.

- **AWS CodeArtifact** implements the Simple Repository API v1.1 through
  [PEP 503, PEP 691, and PEP 700](https://docs.aws.amazon.com/codeartifact/latest/ug/python-compatibility.html). It
  omits the root `/simple/` project list, the Warehouse `/pypi/` JSON API, and XML-RPC. Authorization tokens last from
  15 minutes to 12 hours, then
  [must be fetched again](https://docs.aws.amazon.com/codeartifact/latest/ug/tokens-authentication.html).
- **GitLab's PyPI registry** publishes packages at project and group endpoints and
  [forwards misses to pypi.org by default](https://docs.gitlab.com/user/packages/pypi_repository/). Its caching
  [virtual registry supports Maven and container formats](https://docs.gitlab.com/user/packages/virtual_registry/#supported-package-formats),
  not PyPI.
- **Azure Artifacts** stores a package in the feed after an authorized client first installs it from an
  [upstream source](https://learn.microsoft.com/en-us/azure/devops/artifacts/concepts/upstream-sources?view=azure-devops).
  Python feeds can use PyPI as a public upstream; custom upstream sources remain limited to npm.
- **Google Artifact Registry** offers standard, remote, and virtual Python repositories. A remote repository
  [caches packages from its upstream](https://cloud.google.com/artifact-registry/docs/python/manage-packages); a virtual
  repository searches its upstreams by configured priority without storing packages itself.

For upstream credentials, follow the provider's current setup for
[GitLab tokens](https://docs.gitlab.com/user/packages/pypi_repository/#authenticate-with-the-gitlab-package-registry),
[Azure Artifacts](https://learn.microsoft.com/en-us/azure/devops/artifacts/python/project-setup-python?view=azure-devops),
or [Google Artifact Registry](https://cloud.google.com/artifact-registry/docs/python/authentication).

## Peryx differences

Peryx serves [PEP 658, PEP 691, and PEP 700](@/ecosystems/pypi/reference/standards.md) from each PyPI index. When the
cloud registry must remain the upload target, configure it as a
[private cached index](@/ecosystems/pypi/guides/private-mirror.md). Peryx keeps the downstream route and cache local;
the cloud registry keeps package ownership and upstream authorization.

## Configuration mapping

| Registry        | Its simple URL                                                                      | As a peryx cached index                                        |
| --------------- | ----------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| CodeArtifact    | `https://{domain}-{acct}.d.codeartifact.{region}.amazonaws.com/pypi/{repo}/simple/` | `credential_exec` returning a fresh Basic or bearer token      |
| GitLab          | `https://host/api/v4/projects/{id}/packages/pypi/simple`                            | `username` and `password` for a personal, deploy, or job token |
| Azure Artifacts | `https://pkgs.dev.azure.com/{org}/{proj}/_packaging/{feed}/pypi/simple/`            | `username` and `password` for a PAT                            |
| Google AR       | `https://{loc}-python.pkg.dev/{proj}/{repo}/simple/`                                | `credential_exec` or documented Basic authentication           |

## Pitfalls

- Configure [`credential_exec`](@/core/operations/configuration.md#exec-credential-helpers) for expiring upstream
  credentials. Its response includes the expiry, so peryx refreshes the credential before the next upstream request.
- Cloud IAM does not authorize Peryx's downstream routes. Configure
  [anonymous or protected reads](@/core/access/control-access.md) and scoped client tokens separately.
- A peryx cache changes client traffic, not provider billing rules. Check the provider's current storage, request, and
  data-transfer prices before keeping the registry as an upstream.
