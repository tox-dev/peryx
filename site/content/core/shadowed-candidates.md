+++
title = "Shadowed candidates"
description = "Explain how a virtual repository chooses one candidate from overlapping members."
weight = 11
+++

A [virtual repository](@/core/indexes.md) resolves candidates from ordered member repositories. An earlier member or a
fallback policy can hide a candidate supplied by another member. The selected candidate appears in client responses;
shadowed candidates remain available to an inspection surface when the ecosystem driver supports one.

Candidate identity, precedence, policy fields, request parameters, and response formats depend on the ecosystem.

- [Inspect shadowed Python package candidates](@/ecosystems/pypi/reference/shadowed-candidates.md)
- [OCI registry behavior](@/ecosystems/oci/reference/registry-behavior.md)

Inspection reads stored repository state and does not change member order or policy decisions. See
[Repository roles](@/core/indexes.md) for virtual resolution and [Policy decisions](@/core/policy-decisions.md) for the
shared decision model.
