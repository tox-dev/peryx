+++
title = "Use a local HTTP registry"
description = "Why loopback container registries work over HTTP and what changes across a VM or network boundary."
weight = 12
+++

Container clients treat a loopback registry as local, so `localhost` and `127.0.0.0/8` work over plain HTTP without a
registry certificate. This is suitable for a client and peryx running in the same network namespace.

The exception does not cross a network or VM boundary. Docker Desktop and similar engines run in a VM, where the host's
`localhost` is not the engine's loopback address. A registry reached by hostname or non-loopback address needs HTTPS or
an explicit insecure-registry setting.

For production, configure [HTTPS](@/core/operations/serve-https.md). For local testing, `podman` and `crane` accept
per-command insecure flags; Docker uses its daemon `insecure-registries` setting.

See [run a container registry](@/ecosystems/oci/guides/container-registry.md) for complete client commands.
