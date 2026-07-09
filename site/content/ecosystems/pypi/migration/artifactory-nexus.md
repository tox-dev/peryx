+++
title = "From Artifactory or Nexus"
description = "Keep them for the other ecosystems if you must; serve Python from one small binary with the modern protocols first."
weight = 6
[extra]
logos = [ "logos/jfrog.svg", "logos/sonatype.svg"]
+++

[Artifactory](https://jfrog.com/artifactory/) and
[Sonatype Nexus Repository](https://www.sonatype.com/products/nexus-repository) are multi-format repository managers:
local, remote (proxy), and virtual (group) repositories for every ecosystem at once, with enterprise auth and support
behind them. Both are sized for that breadth: Artifactory documents a
[minimum of 8 GB RAM for production](https://docs.jfrog.com/installation/docs/system-requirements), Nexus
[2 CPUs and 8 GB with Java 21](https://help.sonatype.com/en/sonatype-nexus-repository-system-requirements.html), and
both gate features by edition: PyPI support is absent from Artifactory's OSS build entirely, and Nexus Community Edition
caps usage at 40,000 components or 100,000 requests per day.

## Why peryx

For the Python slice of the job, protocol support is the concrete difference. Nexus shipped PEP 658 metadata and the PEP
691 JSON API in [3.93.0 (June 2026)](https://help.sonatype.com/en/sonatype-nexus-repository-3-93-0-release-notes.html);
Artifactory added opt-in PEP 691 in 7.146.7 (April 2026) and still has
[no PEP 658 support](https://jfrog.atlassian.net/si/jira.issueviews:issue-html/RTFACT-26891/RTFACT-26891.html), with the
request open since 2022; every resolve against it downloads wheels to read their metadata. peryx serves both by
default, backfills them for upstreams that lack them, and idles in tens of megabytes of RAM.

If the rest of the organization stays on Artifactory or Nexus, peryx can also sit in front: configure the existing
repository as a [cached index with credentials](@/ecosystems/pypi/guides/private-mirror.md), and clients get the JSON
and metadata fast paths the upstream does not offer.

## The renames

| Artifactory / Nexus                          | peryx                         |
| -------------------------------------------- | ------------------------------- |
| remote repository                            | cached index                    |
| local / hosted repository                    | hosted index                    |
| virtual / group repository                   | virtual index                   |
| `…/api/pypi/{repo}/simple` (Artifactory)     | `/{route}/simple/`              |
| `…/repository/{repo}/simple` (Nexus)         | `/{route}/simple/`              |
| deploy via UI or REST                        | `twine upload` / `uv publish`   |
| access tokens, user tokens (Nexus: Pro only) | `upload_token` per hosted index |

## Pitfalls

- Fewer ecosystems: peryx serves the ecosystems listed under [Ecosystems](@/ecosystems/_index.md); repositories in an
  ecosystem peryx does not yet serve stay where they are.
- No LDAP/SSO, per-user permissions, HA clustering, or lifecycle/cleanup policies.
- Their virtual repositories can include many members with priority rules; virtual indexes compose the same way, but
  member-specific routing rules (per-pattern includes) become separate virtual routes.
