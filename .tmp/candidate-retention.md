Created accepted issue: https://github.com/tox-dev/velodex/issues/32

Candidate 6: Quotas, Retention, And Cleanup Policies

Status: possible new issue Priority: P1 Labels: type:feature, area:storage, area:management, area:cli, area:performance

Velox needs repository-level limits and cleanup rules for stored packages. This covers hosted packages, cached upstream
packages, metadata siblings, and orphan blobs left after failed writes.

This is separate from #28. The policy engine decides whether a package may enter or serve. Retention and cleanup decide
which stored content should expire after it already exists.

Why it is useful:

- Prevents one package, mirror, or team from filling disk or object storage.
- Lets operators keep latest-N releases while deleting old cache entries.
- Makes cleanup predictable with dry-run output before deletion.
- Gives #31 full mirrors a way to prune package sets after upstream changes.
- Gives #32 object storage a way to detect and remove orphan blobs.

Why it belongs in proposal.md:

Cloud and repository-manager competitors expose cleanup as a product feature. Google Artifact Registry supports delete
policies, keep policies, keep-most-recent versions, package prefixes, age filters, and dry runs. Nexus supports cleanup
policies for hosted and proxy repositories, preview reports, component age, component usage, retained versions, and
asset name matching. GitLab documents PyPI package size limits.

References:

- https://docs.cloud.google.com/artifact-registry/docs/repositories/cleanup-policy
- https://help.sonatype.com/en/cleanup-policies.html
- https://docs.gitlab.com/user/packages/pypi_repository/

MVP scope I would propose:

1. Add per-repository quota fields: max bytes, max projects, max versions per project, and max file size.
1. Add cleanup policy rules: older-than, keep latest-N versions, package name prefixes, cached-only, hosted-only, and
   orphan blobs.
1. Add velodex cleanup plan <repo> to show deletions without changing storage.
1. Add velodex cleanup apply <repo> to delete only the planned objects after confirmation or --yes.
1. Track enough metadata for age and usage rules: upload time, first cached time, last downloaded time, byte size,
   source repository, hosted versus cached.
1. Prevent deletes from breaking the currently advertised Simple API response.
1. Add tests for dry-run output, hosted retention, cached retention, latest-N ordering with PEP 440, orphan cleanup, and
   object-storage cleanup.

Out of scope for this issue: browser policy editing, scheduled cleanup jobs, billing reports, tenant-specific quotas,
and legal hold.

My take: accept if Velox will run unattended with mirrors or object storage. Defer if operators will handle disk cleanup
outside Velox for now.

Accept, reject, or change scope?
