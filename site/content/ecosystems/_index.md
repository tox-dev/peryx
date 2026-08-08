+++
title = "Ecosystems"
description = "Protocol implementations shipped in the peryx binary."
sort_by = "weight"
template = "section.html"
+++

An ecosystem defines artifact names, metadata, storage encoding, client commands, and wire behavior. Every index pairs
one ecosystem with a [role](@/core/indexes.md): cached, hosted, or virtual. A virtual index may combine only members of
the same ecosystem.

The peryx binary contains the PyPI and OCI implementations. Their crates own all protocol and policy behavior. Select
one below for setup, configuration, and behavior. The [capability matrix](@/ecosystems/capabilities.md) compares
supported roles and shared features.
