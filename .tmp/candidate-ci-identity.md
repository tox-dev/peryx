Created accepted issue: https://github.com/tox-dev/velodex/issues/34

Candidate 8: CI Identity Publishing

Status: possible new issue Priority: P1 Labels: type:feature, area:auth, area:upload, area:security, area:api

CI identity publishing lets a CI job publish packages without a long-lived Velox upload token stored as a secret. The CI
provider sends an OIDC identity token to Velox, Velox verifies it against a configured trusted publisher rule, and Velox
mints a short-lived upload credential for the matching project or repository.

This is related to #22, but it is narrower. #22 covers scoped tokens, read ACLs, and credential refresh. This issue
covers the publish flow that turns a CI identity into a temporary upload credential.

Why it is useful:

- Removes long-lived upload tokens from CI secret stores.
- Binds a publish permission to repository, branch, tag, workflow, environment, or service account claims.
- Limits credential lifetime to minutes instead of months.
- Gives #34 security event logs a stable actor such as `github:org/repo:workflow:release`.
- Makes Velox easier to use with GitHub Actions, GitLab CI/CD, and cloud build services.

Why it belongs in proposal.md:

PyPI Trusted Publishing uses OIDC so automated environments can publish without manually generated API tokens. PyPI
exposes an audience endpoint and a token-minting endpoint, then returns a short-lived API token for normal upload tools.
GitLab also documents CI job tokens for PyPI registry access and Trusted Publishing examples that use GitLab OIDC ID
tokens.

References:

- https://docs.pypi.org/trusted-publishers/
- https://docs.pypi.org/trusted-publishers/using-a-publisher/
- https://docs.gitlab.com/user/packages/pypi_repository/
- https://packaging.python.org/en/latest/specifications/index-hosted-attestations/

MVP scope I would propose:

1. Add trusted publisher config for issuer, audience, subject pattern, repository, workflow/environment claims, target
   Velox repository, and allowed project names.
1. Add `GET /_/oidc/audience` so publishing actions can discover the expected audience.
1. Add `POST /_/oidc/mint-token` that verifies an OIDC token and returns a short-lived project-scoped upload token.
1. Make minted tokens work with twine and uv publish through the existing upload API.
1. Require scoped upload permission and short expiry for minted tokens.
1. Log success and failure events through #34.
1. Add tests with fixture OIDC tokens for accepted issuer, wrong audience, wrong subject, expired token, wrong project,
   and upload success.

Out of scope for this issue: user login OIDC, browser SSO, attestation verification, hosted provenance display, and
CI-specific UI setup wizards.

My take: accept if we want Velox to handle production publishing. Defer if upload tokens are enough for the first
release.

Accept, reject, or change scope?
