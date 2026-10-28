# Security policy

peryx is an artifact cache and private registry that serves Python packages and OCI images from one executable. This
policy says how to report a security problem and how we decide whether a report is one.

## Reporting a vulnerability

**Do not open a public issue for a security problem.** Use
[GitHub's private vulnerability reporting](https://github.com/tox-dev/peryx/security/advisories/new). If neither works
for you, email [gaborjbernat@gmail.com](mailto:gaborjbernat@gmail.com).

Include what the attacker controls and what they gain, the steps or a proof of concept, and the versions and platform
you tested. Write the report yourself; unreviewed tool or model output takes longer to triage.

## What to expect

Volunteers maintain peryx, with no on-call rotation. We aim to acknowledge a report within **5 working days** and assess
it within **15 working days**. Fixes are developed in a private fork attached to the draft advisory, ship in a new
release, and the advisory, with a CVE requested through GitHub, goes out with that release. The default embargo is **90
days** from the report; we agree any change with the reporter. We credit reporters in the advisory under the name they
choose, or not at all. There is no bug bounty.

## Supported versions

Only the latest release receives fixes. Confirm the problem reproduces on it before reporting.

## Vulnerability or bug

A report is a vulnerability when an attacker who controls only an input that the [threat model](THREAT_MODEL.md) lists
as untrusted breaks a guarantee peryx makes. A report that first needs control of something the threat model trusts,
such as the operator's configuration, an administrator account, or the host peryx runs on, is a bug: we fix it in the
open and publish no advisory. Classify by who supplies the dangerous input, not by how bad the outcome sounds.

**In scope**

- A request reaching an action (`read`, `write`, `delete`) its token's grant or the index ACL does not allow, including
  through a virtual index, a route of another ecosystem, or the web UI.
- A client credential being forwarded to an upstream, or an upstream credential reaching a client, so cached private
  data leaks across indexes.
- Content served under a digest it does not match, from an upstream, a peer datacenter or object storage.
- A scoped token granting more than its creator holds, or surviving revocation or expiry.
- A revoked digest still served, a webhook delivered unsigned, or to a target the index did not configure.
- Memory exhaustion, unbounded work or a crash an unauthenticated client can trigger with small requests.

**Out of scope**

- Settings the operator chooses: anonymous reads, the upstreams, ACLs, TLS or the lack of it, and who holds admin
  credentials.
- Malicious packages or images that an upstream serves under their own correct digests; peryx caches what the upstream
  publishes.
- Attacks that need write access to the host, the data directory, object storage credentials or the replication token.
- Benchmarks, test fixtures and the `peryx-bench` tooling.

If you cannot tell which side a finding falls on, report it privately and we will decide.
