+++
title = "Ecosystems"
description = "Which packaging formats peryx serves: PyPI and OCI. Every index is a role paired with an ecosystem."
sort_by = "weight"
template = "section.html"
+++

An **ecosystem** is a packaging format and the wire protocol that carries it: how clients ask for packages, how versions
and filenames are shaped, and what an "artifact" (an installable file) looks like. PyPI (Python packages) is one
ecosystem; OCI (container images) is another.

peryx treats the ecosystem as a first-class axis. Every index you configure is a **role** (what it does: cached,
hosted, or virtual; see [the index model](@/core/indexes.md)) paired with an **ecosystem** (which format it speaks). The
two are independent: the same three roles work for every ecosystem, and a [virtual index](@/core/glossary.md#roles) may
only combine members of the same ecosystem.

Today peryx ships two ecosystems: PyPI and OCI (container images). The architecture is built so more plug in without
reshaping the core: each new ecosystem is a driver that teaches peryx that format's protocol and artifact rules. The
[capability matrix](@/core/capabilities.md) tracks what each ecosystem supports.

Pick your ecosystem below for its "Set Me Up" hub: what the role trio means for that format, the wire protocol, and the
client-config snippets to point your tools at peryx.
