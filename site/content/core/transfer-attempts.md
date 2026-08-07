+++
title = "Transfer attempts"
description = "The durable identity, progress, and retry history of the transfers that populate a blob placement, and how a worker resumes one after a restart."
weight = 11
+++

A blob [placement](@/core/blob-placement.md) records where a digest ends up and whether it can serve. A transfer attempt
records the work of getting it there: one current attempt per target placement, the sequence of retries behind it, a
progress checkpoint a restarted worker resumes from, and a classified terminal outcome. The transfer engine, the remote
streaming endpoint, and the operator UI are separate; this page describes the durable state they build on.

An attempt is keyed by the same four parts as its placement : digest, backend, data center, and location : plus a
sequence number. A placement retains up to 32 attempts; a stage that would exceed the bound is refused rather than
growing an unbounded history, and [compaction](#retention) trims the terminal ones a retention policy no longer needs.

## States

An attempt moves through an evidence-based lifecycle. A staged temporary file is never counted as complete; only bytes
that hash to the target digest at the exact object size are.

- **In progress** : bytes are moving. The attempt carries the last *durable* checkpoint offset, which trails the live
  byte position by at most the checkpoint budget.
- **Failed** : the attempt ended without a serveable object, for a classified reason: the source was unavailable, the
  delivered bytes hashed to a different digest, or the backend refused the write. A source-unavailable failure is
  answered with a retry or a reselected source; a digest mismatch can never serve.
- **Succeeded** : the transfer delivered the exact object size and its digest matched the target.

Completing an attempt with an observed digest that does not match the target, or a byte size that does not match the
expected one, records a digest-mismatch failure rather than a success. A failed attempt is retained as history and never
erases a verified placement in the [placement ledger](@/core/blob-placement.md), because the two are separate records.

## Retries and source reselection

A retry does not mutate the failed attempt; it opens the next sequence for the same placement, so the history shows how
a placement was reached. Each attempt records the data center its bytes were pulled from, so a source reselection after
an unavailable peer is visible in the history rather than hidden inside one mutable row. Every attempt carries a fencing
epoch: a worker applies transitions under the epoch it holds, and a write from a lower epoch is a stale worker that lost
ownership and is rejected without changing the record.

## Progress checkpoints

A high-frequency byte stream must not turn into one metadata write per chunk. A checkpoint therefore persists only when
the offset has advanced past a configured budget : a minimum byte delta or a minimum interval since the last durable
write : and is coalesced without a write in between. The offset that reaches the exact object size always persists, so a
completed transfer's recorded progress is precise. An offset at or below the last durable one never regresses the
checkpoint.

Because the durable checkpoint lags the live position, a restarted worker resumes from the last checkpoint and re-reads
a bounded amount of already-transferred bytes rather than restarting the object. Reopening the store and beginning the
same placement returns the interrupted in-progress attempt unchanged, with its last durable offset, instead of opening a
new sequence.

## Retention

Compaction removes terminal attempts a retention policy no longer requires, in bounded batches so a large backlog never
holds one long transaction. It keeps every in-progress attempt and, per placement, the configured number of most-recent
terminal attempts regardless of age; older terminal attempts past that count are pruned once they age out of the
configured window.

## Metrics

Transfer attempts aggregate into bounded per-label counts for a metrics exporter. Series are labeled by data center,
backend, state, and : for failures : error class. The digest, location, sequence, and operation identity are excluded,
so cardinality stays within the topology's backends and data centers times the fixed state and failure classes rather
than growing one series per artifact or transfer.
