## Summary

Make hosted, proxy, and virtual PyPI repositories first-class Velox concepts.

## Problem

Velox has local, mirror, and overlay indexes in configuration. Those cover the basic mechanics, but they do not give
operators a product-level repository model with stable API semantics, member ordering, source precedence, upload
routing, source visibility, and conflict handling.

That makes later management APIs, policy controls, and UI work harder to define. Operators need a clear answer to which
repository served a package, which member shadowed another file, and where uploads land when users publish through a
combined endpoint.

## Competitor reference

Nexus documents PyPI proxy, hosted, and group repositories. Group repositories combine proxy and hosted repositories and
expose them through one URL with member repositories in a chosen order.

AWS CodeArtifact lets one repository use upstream repositories so a client can access packages from more than one
repository through one endpoint.

References:

- https://help.sonatype.com/en/create-a-pypi-repository.html
- https://docs.aws.amazon.com/codeartifact/latest/ug/repos-upstream.html

## Proposed scope

- Add public repository concepts:
  - `hosted`: local packages users upload to Velox
  - `proxy`: cached upstream packages
  - `virtual`: ordered merge of hosted and proxy repositories
- Preserve compatibility with existing `local`, `mirror`, and `overlay` config.
- Add ordered members for virtual repositories.
- Define upload target rules for virtual repositories.
- Define source precedence and shadowing rules.
- Expose source and shadowing state in status, management API responses, and web UI data.
- Make policy checks from #28 and #29 run against the resolved repository member and source.
- Add pip and uv tests for:
  - member ordering
  - local shadowing
  - upstream fallback
  - upload routing through a virtual repository

## Out of scope

- browser-based repository editing
- tenant ownership and RBAC
- HA and replication
- object storage
- scanner policy

## Acceptance criteria

- Operators can define hosted, proxy, and virtual repositories without losing existing config compatibility.
- A virtual repository serves one Simple API endpoint that merges ordered members deterministically.
- Uploads through a virtual repository land in the configured hosted target or fail with a clear error.
- API and UI responses identify the source repository for files and mark shadowed files.
- Policy evaluation can distinguish local hosted files, proxied upstream files, and virtual repository decisions.
- pip and uv tests cover the main resolution and upload paths.
