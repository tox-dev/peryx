## Summary

Add backup, restore, and directory import commands for Velox state.

## Problem

Velox can host private uploads and cache upstream artifacts, but it does not provide a supported recovery path for
metadata and blobs. Operators need to move a Velox instance, recover from disk failure, and import existing package
files without bypassing validation.

Velox remains the serving layer. This issue does not add static mirror serving.

## Competitor reference

Repository managers usually provide backup/restore or import/export workflows. Nexus documents repository import/export
tasks and backup/restore workflows.

Bandersnatch supports diff files for offline transfer, which addresses a different mirror-transfer use case.

References:

- https://help.sonatype.com/en/repository-management.html
- https://github.com/pypa/bandersnatch/blob/main/docs/mirror_configuration.md

## Proposed scope

- Add `peryx backup create <path>`.
- Include:
  - redb metadata
  - config snapshot
  - referenced blob digests
  - referenced blob bytes
- Support full backup first.
- Add `peryx backup verify <path>`.
- Add `peryx restore <path> --data-dir <dir>`.
- Refuse restore into a non-empty data directory unless `--force` is passed.
- Add `peryx import-dir <repo> <dir>` for local wheels and sdists.
- Run imported artifacts through upload validation from #2, #3, and #4.
- Report imported, skipped, and rejected files with filenames and reasons.
- Add tests for:
  - backup/restore round trip
  - missing blob detection
  - config mismatch warnings
  - restore into non-empty directory
  - import-dir validation

## Out of scope

- static serving
- hot online backups
- incremental backups
- cross-version migrations
- cloud-native snapshots
- scheduled backup jobs

## Acceptance criteria

- Operators can create a full backup that contains metadata and referenced blobs.
- Backup verification detects missing or mismatched blobs.
- Restore into a fresh data directory produces a Velox instance that serves the same hosted packages.
- Restore refuses unsafe targets unless forced.
- Directory import validates wheels and sdists before adding them to a hosted repository.
