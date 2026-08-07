+++
title = "Troubleshoot Docker and Podman"
description = "Diagnose token challenges, missing manifests, push limits, and availability retries."
weight = 90
+++

`unauthorized: authentication required` means the repository restricts access and the request has no valid registry
token. The client should follow the `WWW-Authenticate` Bearer challenge to `/v2/token`.

Token endpoint failures identify their cause:

- `token authentication is not enabled`: configure a signing key.
- `requested service is not available`: use the service value from the registry challenge.
- `invalid credentials`: provide a live Basic credential to the token endpoint.

A registry request with an expired or invalid bearer token returns `401` and `error="invalid_token"`. A valid token
without the requested repository action returns `401` and `error="insufficient_scope"`. Refresh the token for the first
case; change the grant for the second.

## Missing and upstream content

An unknown repository or tag returns `404` with `MANIFEST_UNKNOWN`. A missing blob returns `BLOB_UNKNOWN`. An upstream
pull-through failure without stored content returns `502`.

## Replica and fencing responses

A read-only replica refuses blob and manifest mutations with `503 Service Unavailable`. A request under a superseded
repository epoch returns `503` with code `UNAVAILABLE`. Retry the same operation against the current writer. Resumable
blob uploads retain their session and staged bytes across this refusal.

See [Availability behavior](@/ecosystems/oci/reference/availability.md).

## Push quotas

A blob, mount, or manifest reservation that crosses a quota returns `403 DENIED`. Peryx publishes no repository
membership, manifest, or tag for the refused operation. Audit mode records the denial and accepts the push.

See [OCI policy settings](@/ecosystems/oci/reference/policy.md#push-quotas) and
[Token authentication](@/ecosystems/oci/reference/token-auth.md).
