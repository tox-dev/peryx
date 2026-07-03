## Summary

Define a security event logging contract for repository actions.

## Problem

Velox has operational logs, but it does not define which repository and security actions must be logged or which fields
must appear on those events. Operators can use Splunk, Humio, Loki, grep, or another log system for queries, but the
logs need stable fields so those tools can reconstruct what happened.

This issue replaces a separate audit-log store for now. Velox should keep using the existing logging pipeline and make
the security-relevant events reliable.

## Competitor reference

Google Artifact Registry documents admin, data-read, and data-write audit events, including Python install and upload
actions.

Nexus writes JSON audit log records when users or internal processes modify configuration or add/remove assets and
components.

References:

- https://docs.cloud.google.com/artifact-registry/docs/audit-logging
- https://help.sonatype.com/en/auditing.html

## Proposed scope

- Define required log events for:
  - upload
  - delete
  - yank
  - restore
  - policy denial
  - cleanup apply
  - mirror sync
  - token use
  - token change
  - repository config change
- Use stable fields where the data exists:
  - action
  - result
  - actor or token ID
  - repository
  - project
  - version
  - filename
  - digest
  - source IP
  - user agent
  - request ID
  - error reason
- Emit success and failure events with the same field names.
- Redact credentials, bearer tokens, Basic auth values, and URL secrets.
- Add tests that assert required fields exist on representative success and failure paths.
- Document sample grep and structured-log queries.

## Out of scope

- separate audit storage
- audit query CLI
- audit UI
- signed audit logs
- SIEM-specific integrations

## Acceptance criteria

- Security-relevant repository actions produce structured log lines with stable event names and field names.
- Failed and denied actions are logged with enough context to identify actor, target, and reason.
- Logs do not expose credentials or token values.
- Tests fail if required security-event fields disappear from representative paths.
- Documentation shows how to query the events with plain grep and a structured log system.
