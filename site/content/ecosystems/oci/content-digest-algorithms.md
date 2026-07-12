+++
title = "Why peryx accepts a non-sha256 content digest"
description = "Why peryx content-addresses under its own sha256 yet accepts an upstream that advertises a manifest digest in another algorithm, and the failed pull that scoping the integrity check prevents."
weight = 6
+++

The [image-spec digest grammar](https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests) is
`algorithm:encoded`, and it names more than one algorithm. `sha256` is the common one, `sha512` is registered beside it,
and the grammar leaves room for others. A registry is free to advertise its content address in any of them through the
`Docker-Content-Digest` header. peryx has to read what an upstream sends, not only what it would have sent itself.

## The pull that used to fail

peryx stores a manifest byte-for-byte and addresses it by the sha256 of those exact bytes. When it pulls a manifest
through, it computes that sha256 and, if the upstream advertised a digest, compares the two. The comparison catches a
corrupting proxy or CDN between peryx and the upstream: altered bytes hash to something else, so a mismatch means the
manifest is not what upstream signed for, and peryx refuses to cache it.

That comparison was a plain string equality. An upstream that content-addresses with sha512 advertises `sha512:6910c9…`;
peryx computed `sha256:fc6b27…` over byte-identical content and compared the two strings. They can never be equal, a
different algorithm and a different length, so peryx read every such pull as a corrupted hop, returned `502`, and cached
nothing. A registry that did nothing wrong was unusable through peryx, and no retry could fix it, because the "mismatch"
was structural.

## What the check is actually for

The integrity check earns its place only when peryx can recompute the advertised digest. For `sha256:` it can: it hashes
the bytes itself and a mismatch is real evidence of tampering. For `sha512:` it cannot, because it does not hash the
bytes a second time under sha512, so comparing a sha512 string to a sha256 string proves nothing about the bytes.
Treating that guaranteed inequality as corruption was the bug.

So the check is scoped to a `sha256:` advertisement, the case where it can run. A digest in any other algorithm is not
compared; peryx content-addresses the bytes under its own sha256, which it still computes and verifies, stores them, and
serves them. A wrong `sha256:` advertisement is still rejected exactly as before, because there the comparison is
meaningful.

## Why this keeps the guarantee

peryx's own integrity promise does not change. It still hashes every manifest it stores, serves it under that sha256,
and reports that sha256 in `Docker-Content-Digest`, so a client that pulls the manifest back verifies the digest peryx
computed over the bytes it holds. The only thing dropped is a comparison that could not run in the first place. A pull
by a non-sha256 digest is served under the digest the client asked for, the upstream's content address, while the cache
key stays peryx's sha256.

## The failure it prevents, and the scope

Without this, an entire class of upstream, any registry or client that content-addresses with sha512 or another
registered algorithm, returns `502` on every tag, and interop with a spec-conformant registry breaks on a detail the
spec explicitly allows. Accepting the broader grammar is what lets peryx sit in front of one.

The relaxation is narrow. It applies to the online manifest pull-through path. Blobs are still sha256 only, an offline
mirror still pins a by-digest reference to sha256, and a malformed digest is still rejected. The exact rules are in
[content digest algorithms](@/ecosystems/oci/reference/content-digests.md); the surrounding read path is in
[how peryx scopes and serves manifest reads](@/ecosystems/oci/manifest-serving.md).
