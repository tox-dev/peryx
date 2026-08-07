+++
title = "Artifact source and availability"
description = "Two typed dimensions peryx records for every artifact: where its bytes came from, and whether this instance can serve them without an upstream fetch. The transition table, the repair pass, and the storage guarantees behind them."
weight = 9
+++

peryx records two independent facts about each artifact, apart from policy, yank, trash, and
[revocation](@/core/digest-revocations.md). **Source** is where the bytes came from. **Byte availability** is whether
this instance can serve them right now. A package read resolves both from one indexed lookup, so a listing never probes
the content store per artifact.

The two dimensions are independent. A proxied artifact may have cached bytes; a hosted artifact is local until its bytes
are lost. Neither says anything about whether a policy permits the download, whether the publisher yanked the release,
or whether an administrator revoked the digest. A read applies those dimensions after source and availability.

## Glossary

**Source** is where an artifact's bytes originate. Caching or evicting the bytes does not change it.

- `hosted`: a publisher sent the artifact to this instance. No upstream can resupply lost bytes.
- `proxy`: peryx cached the artifact from an upstream index. A local miss triggers an upstream fetch.
- `generated`: this instance produced the artifact, such as a rendered index page or a derived metadata sibling. A local
  miss triggers regeneration.

**Byte availability** records whether this instance can serve the bytes now. The events below keep this projection in
step with the content store.

- `local`: the configured storage holds verified bytes; a read serves them without an upstream fetch.
- `remote_only`: a known upstream can supply bytes missing from local storage.
- `unavailable`: local storage has no bytes and no upstream can supply them.

`local` means verified, complete bytes. [Metadata](@/core/glossary.md#artifact) alone does not make an artifact local.
Neither does a partial transfer that fails digest verification.

## Transition table

An artifact's placement starts when it is first recorded and moves only along byte-availability. The source is fixed at
recording time. `has upstream` is true only for a `proxy` source.

| Event                                                                        | New availability                             |
| ---------------------------------------------------------------------------- | -------------------------------------------- |
| Recorded with verified local bytes (publish, generate, completed cache fill) | `local`                                      |
| Recorded without local bytes (discovered upstream)                           | `remote_only` if `proxy`, else `unavailable` |
| Verified bytes written                                                       | `local`                                      |
| Local bytes removed (eviction)                                               | `remote_only` if `proxy`, else `unavailable` |
| Write or cache fill failed                                                   | unchanged                                    |
| Repaired, bytes observed present                                             | `local`                                      |
| Repaired, bytes observed absent                                              | `remote_only` if `proxy`, else `unavailable` |

The failed-write row preserves the prior placement. A failed cache fill cannot drop a verified `local` copy or create
one from a partial transfer. A metadata fetch or truncated download cannot produce `local`.

## Repair

The availability projection can drift from the content store when an operator removes a blob out of band or a fill
crashes between writing bytes and recording them. A repair pass reads a bounded batch of placements in digest order,
checks each digest's byte presence, and rewrites mismatched rows. Its return cursor resumes the next batch, which keeps
the fixed-size work off the request path.

Repair changes only the availability projection. It does not read or write source, policy, yank, trash, or revocation,
so a stale-projection repair cannot alter an access decision. The batch cap prevents a repair pass from holding a read
span over the whole table.

## Storage guarantees

- **One indexed lookup.** Source and availability live in one record keyed by content digest. A package read resolves
  both without a per-artifact call into the content store.
- **Verified-only local.** A placement reaches `local` only after its bytes are written and verified against their
  digest. The content store is content-addressed, so anything present is by construction correct.
- **Source is intrinsic.** Caching, evicting, or repairing an artifact does not rewrite its source. Only a different
  artifact taking the digest's place does.
- **Availability is a projection.** A repair pass can reconstruct this derived state from the content store without
  upstream coordination.

## Cache-failure behavior

A cache fill streams upstream bytes into a pending blob and commits only when the digest verifies. The placement update
follows the outcome:

- The fill verifies and commits; the proxied artifact becomes `local`.
- The fill fails, aborts, or verifies to the wrong digest; the placement remains unchanged. A prior `local` copy stays
  `local`, while an artifact without local bytes stays `remote_only`.

A store error while updating the projection does not fail the fill. The disk holds verified bytes; the next repair pass
reconciles the projection. A transient metadata-store fault therefore cannot fail a download or corrupt the source
dimension.

## API schema

The typed placement serializes with stable `snake_case` spellings, so a client matches on a value rather than parsing
prose:

```json
{
  "source": "proxy",
  "availability": "remote_only"
}
```

`source` is one of `hosted`, `proxy`, `generated`. `availability` is one of `local`, `remote_only`, `unavailable`. Any
`source` pairs with any `availability` its transitions allow.
