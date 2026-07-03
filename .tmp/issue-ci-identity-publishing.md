## Summary

Add CI identity publishing with OIDC token minting for uploads.

Depends on #22. Related to #34.

## Problem

Velox supports upload tokens, but CI systems still need a long-lived secret to publish packages. That makes token
rotation, secret exposure, and project scoping harder than they need to be.

Velox should accept a CI identity token, verify it against trusted publisher rules, and mint a short-lived upload
credential for the matching project or repository.

## Competitor reference

PyPI Trusted Publishing uses OIDC so automated environments can publish without manually generated API tokens. PyPI
exposes an audience endpoint and a token-minting endpoint, then returns a short-lived API token for normal upload tools.

GitLab documents CI job tokens for PyPI registry access and Trusted Publishing examples that use GitLab OIDC ID tokens.

References:

- https://docs.pypi.org/trusted-publishers/
- https://docs.pypi.org/trusted-publishers/using-a-publisher/
- https://docs.gitlab.com/user/packages/pypi_repository/

## Proposed scope

- Add trusted publisher config for:
  - issuer
  - audience
  - subject pattern
  - workflow or environment claims
  - target Velox repository
  - allowed project names
- Add `GET /_/oidc/audience`.
- Add `POST /_/oidc/mint-token`.
- Verify OIDC issuer, audience, expiry, subject, and configured claims.
- Return a short-lived project-scoped upload token.
- Make minted tokens work with `twine` and `uv publish` through the existing upload API.
- Require scoped upload permission and short expiry for minted tokens.
- Log success and failure events through #34.
- Add tests for:
  - accepted issuer
  - wrong audience
  - wrong subject
  - expired token
  - wrong project
  - upload success with a minted token

## Out of scope

- user login OIDC
- browser SSO
- attestation verification
- hosted provenance display
- CI-specific setup wizards

## Acceptance criteria

- CI jobs can exchange a valid OIDC token for a short-lived upload token.
- Invalid issuer, audience, subject, expiry, or claim values fail with actionable errors.
- Minted tokens are scoped to configured repositories and project names.
- Minted tokens work with existing Python publishing clients.
- Token minting and upload attempts produce security event logs with stable actor fields.
