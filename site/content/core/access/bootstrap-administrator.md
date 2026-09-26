+++
title = "Bootstrap the first administrator"
description = "Create one local administrator from standard input or a mounted secret before starting peryx."
weight = 11
+++

`peryx bootstrap-administrator` creates the first local administrator in a data directory. Run it before `peryx serve`
to choose the administrator's name and password, or on a `dc` or `ha` node, where `peryx serve` never creates one. The
command opens the metadata store and exits after the transaction; it does not bind a socket or start scheduled jobs.

## Generated administrator

With `availability.mode = "none"` and a writable data directory, `peryx serve` creates an administrator named `admin`
when the metadata store holds no administrator grant. It draws 192 bits from the operating system CSPRNG, encodes them
as a 32-character URL-safe password, and writes that password to `initial-admin-password` in the data directory before
the account commits. On Unix the file has mode `0600`. The account goes through the same transaction as
`peryx bootstrap-administrator`, so it gets the same Argon2id verifier, server administrator grant, and lifecycle
record.

The startup banner adds a `created administrator admin, password in <path>` line below the listening address, and the
log records the same path in its `password_file` field. Neither prints the password. With the default data directory:

```console
$ cat peryx-data/initial-admin-password
```

Store the password in a password manager, then delete the file. Peryx does not remove it: once the administrator grant
exists, later starts skip this step and leave the data directory alone.

Startup fails without creating the account when the file already exists, for example after an interrupted first start
left it behind. Delete the stale file and start again. Startup also fails when a user named `admin` exists without an
administrator grant; bootstrap a differently named administrator with `peryx bootstrap-administrator` instead.

A read-only process, a `dc` or `ha` node, and a data directory that already holds an administrator grant, including one
from `peryx bootstrap-administrator`, create nothing.

The password must contain 15 to 1,024 Unicode characters. Peryx accepts spaces and other Unicode characters without
composition rules, following the
[OWASP authentication guidance](https://cheatsheetseries.owasp.org/cheatsheets/Authentication_Cheat_Sheet.html). Secret
input has a 1 MiB byte limit and must use UTF-8. Peryx removes one terminal LF or CRLF from standard input and secret
files; it preserves every other character.

## First start

Read the password without placing it in shell history or the process arguments:

```console
$ peryx init --data-dir /var/lib/peryx
$ read -rs PERYX_BOOTSTRAP_PASSWORD
$ printf '%s\n' "$PERYX_BOOTSTRAP_PASSWORD" | peryx bootstrap-administrator admin --data-dir /var/lib/peryx --password-stdin
$ unset PERYX_BOOTSTRAP_PASSWORD
administrator	usr_...	admin
$ peryx serve --data-dir /var/lib/peryx
```

The shell variable stays local unless the operator exported it. The `peryx` process receives the password through
standard input, so tools that inspect process arguments cannot read it.

## Secret file

Create the file with access limited to the account that runs peryx, then pass its path:

```console
$ install -m 600 /dev/null /run/peryx-administrator-password
$ read -rs PERYX_BOOTSTRAP_PASSWORD
$ printf '%s\n' "$PERYX_BOOTSTRAP_PASSWORD" > /run/peryx-administrator-password
$ unset PERYX_BOOTSTRAP_PASSWORD
$ peryx bootstrap-administrator admin --data-dir /var/lib/peryx --password-file /run/peryx-administrator-password
$ rm /run/peryx-administrator-password
```

Peryx reads the file once. The lifecycle record contains the stable user ID, and the security event identifies the
bootstrap action. Both exclude credential material and the source path.

## systemd credentials

[systemd credentials](https://systemd.io/CREDENTIALS/) expose secrets as files under `$CREDENTIALS_DIRECTORY` without
passing their contents through the process environment. A one-shot unit can load the password and bootstrap the same
data directory used by the service:

```ini
[Unit]
Description=Create the first peryx administrator
Before=peryx.service

[Service]
Type=oneshot
User=peryx
LoadCredential=administrator-password:/etc/peryx/administrator-password
ExecStart=/usr/bin/peryx bootstrap-administrator admin --data-dir /var/lib/peryx --password-file %d/administrator-password
```

Run the unit once, confirm it succeeded, then remove its `LoadCredential=` source. A second run fails after the first
administrator grant commits.

## Container first start

Mount the data volume and secret into a short-lived container before starting the long-running container. Replace
`your-peryx-image:tag` with the image that contains the installed `peryx` binary.

```console
$ docker run --rm \
    --volume peryx-data:/var/lib/peryx \
    --volume "$PWD/administrator-password:/run/secrets/peryx-administrator-password:ro" \
    your-peryx-image:tag \
    peryx bootstrap-administrator admin \
      --data-dir /var/lib/peryx \
      --password-file /run/secrets/peryx-administrator-password
$ docker run --rm --volume peryx-data:/var/lib/peryx your-peryx-image:tag peryx serve --data-dir /var/lib/peryx
```

Keep the secret outside the image and remove the host copy after bootstrap.

## Refusal and recovery

The metadata transaction checks for an administrator grant before writing. It creates the user, Argon2id verifier,
server administrator grant, and lifecycle record in one commit. Concurrent commands serialize on the metadata writer, so
one command commits and the other reports that an administrator grant exists. A failed write leaves none of those
records.

The command stops working while any administrator grant exists. This prevents local bootstrap from becoming a second
administrator-creation path after setup; use an authenticated management operation for later accounts and grants.

A lost password does not reopen bootstrap because the administrator grant remains. Use another authenticated
administrator to replace the verifier, or restore a protected metadata backup. If recovery leaves the store with no
administrator grant, run the same secret-file command against that recovered data directory before restarting the
server:

```console
$ peryx bootstrap-administrator recovery-admin \
    --data-dir /srv/peryx-recovered \
    --password-file /run/secrets/peryx-recovery-password
```

Create at least two administrator accounts through authenticated management. An operator with write access to the
metadata directory controls local identity state, so restrict the directory and bootstrap secret to the service account.
