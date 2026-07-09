## Summary

Add repository quotas, retention rules, and cleanup commands.

Depends on #30 and #32. Related to #31 and #27.

## Problem

Velox does not enforce repository size limits or provide a supported cleanup workflow. Stored content can grow through
hosted uploads, read-through cache fetches, mirror syncs, PEP 658 metadata siblings, and failed writes that leave orphan
blobs.

Operators need cleanup rules that are safe to preview, explain, and apply. Manual deletion can break Simple API output
or leave metadata pointing at missing blobs.

## Competitor reference

Google Artifact Registry supports cleanup policies with delete rules, keep rules, keep-most-recent versions, package
prefixes, age filters, and dry runs.

Nexus supports cleanup policies for hosted and proxy repositories with preview reports.

GitLab documents PyPI package upload limits.

References:

- https://docs.cloud.google.com/artifact-registry/docs/repositories/cleanup-policy
- https://help.sonatype.com/en/cleanup-policies.html
- https://docs.gitlab.com/user/packages/pypi_repository/

## Proposed scope

- Add per-repository quota fields:
  - max bytes
  - max projects
  - max versions per project
  - max file size
- Add cleanup policy rules:
  - older-than
  - keep latest-N versions
  - package name prefixes
  - cached files
  - hosted files
  - orphan blobs
- Add `peryx cleanup plan <repo>` to show deletions without changing storage.
- Add `peryx cleanup apply <repo>` to delete planned objects after confirmation or `--yes`.
- Track metadata needed for cleanup:
  - upload time
  - first cached time
  - last downloaded time
  - byte size
  - source repository
  - hosted versus cached
- Prevent cleanup from breaking the active Simple API response.
- Report each planned deletion with repository, project, version, filename, source, bytes, and matched rule.

## Out of scope

- browser policy editing
- scheduled cleanup jobs
- billing reports
- tenant-specific quotas
- legal hold

## Acceptance criteria

- Operators can configure quotas and retention rules per repository.
- Quota failures return clear errors before Velox commits new hosted uploads or mirror artifacts.
- `cleanup plan` explains every planned deletion and writes no storage changes.
- `cleanup apply` removes metadata and blobs in an order that keeps the served index consistent.
- Cleanup can remove orphan blobs left by failed writes.
- Tests cover dry-run output, hosted retention, cached retention, PEP 440 latest-N ordering, orphan cleanup, and
  object-storage cleanup.
