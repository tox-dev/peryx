+++
title = "From Artifactory or Nexus"
description = "Map PyPI repositories, permissions, identity, availability, and cleanup from Artifactory or Nexus to peryx."
weight = 6
[extra]
logos = [ "logos/jfrog.svg", "logos/sonatype.svg"]
+++

[Artifactory](https://docs.jfrog.com/artifactory/docs/pypi-repositories) and
[Nexus Repository](https://help.sonatype.com/en/configure-pypi-with-nexus.html) are multi-format repository managers.
Both offer hosted, proxy, and aggregate repository types for PyPI. peryx covers the same repository shapes for the
ecosystems listed under [Ecosystems](@/ecosystems/_index.md).

## Peryx differences

[Nexus](https://help.sonatype.com/en/configure-pypi-with-nexus.html) 3.93 added PEP 658 and PEP 691, and 3.94 added PEP
700\. [Artifactory](https://docs.jfrog.com/artifactory/docs/pypi-repositories#enable-json-indexing-in-pypi-repositories)
supports the Simple JSON API when an administrator enables JSON indexing. peryx serves
[PEP 658, PEP 691, and PEP 700](@/ecosystems/pypi/reference/standards.md) without a feature switch and synthesizes core
metadata when an upstream does not provide it.

[Artifactory permission targets](https://docs.jfrog.com/administration/docs/permissions) and Nexus
[access control](https://help.sonatype.com/en/access-control.html) bind users or groups to repository permissions. Peryx
[role grants](@/core/access/role-grants.md) authorize management operations. Artifact clients use
[configured access grants](@/core/access/control-access.md) or [managed scoped tokens](@/core/access/scoped-tokens.md)
for `read`, `write`, and `delete` actions.

Check the current product documentation before translating
[Artifactory LDAP](https://docs.jfrog.com/administration/docs/ldap),
[Nexus authentication](https://help.sonatype.com/en/authentication.html),
[Artifactory HA](https://docs.jfrog.com/installation/docs/high-availability),
[Nexus HA](https://help.sonatype.com/en/high-availability-deployment.html),
[Artifactory cleanup](https://docs.jfrog.com/administration/docs/cleanup-policies), or
[Nexus cleanup](https://help.sonatype.com/en/cleanup-policies.html). Editions and deployment requirements differ.

## Configuration mapping

| Artifactory / Nexus                          | peryx                                                                                |
| -------------------------------------------- | ------------------------------------------------------------------------------------ |
| remote / proxy repository                    | cached index                                                                         |
| local / hosted repository                    | hosted index                                                                         |
| virtual / group repository                   | virtual index                                                                        |
| `…/api/pypi/{repo}/simple` (Artifactory)     | `/{route}/simple/`                                                                   |
| `…/repository/{repo}/simple` (Nexus)         | `/{route}/simple/`                                                                   |
| deploy through the UI, REST API, or a client | `twine upload` or `uv publish`                                                       |
| repository permissions, roles, and users     | role grants for management; configured or managed scoped tokens for artifact clients |
| directory or OIDC identity                   | [LDAP or OIDC login with group-to-role mappings](@/core/access/authentication.md)    |
| HA deployment                                | [`[availability]` with `dc` or `ha`](@/core/availability/deployment.md)              |
| scheduled cleanup policy                     | [retention plan preview and export](@/core/repositories/retention.md)                |

## Pitfalls

- Keep repositories for ecosystems that peryx does not serve in their existing manager.
- Peryx LDAP and OIDC identities govern management and UI access. They do not replace the scoped credentials used by
  `pip`, `uv`, and `twine` for artifact requests.
- Artifactory and Nexus can execute scheduled cleanup policies. Peryx retention evaluates and exports a read-only plan;
  it does not apply that plan or schedule deletion.
- Preserve virtual-repository ordering. Map path-specific include or exclude rules to separate peryx routes when layer
  order is not enough.
