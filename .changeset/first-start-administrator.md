---
default: minor
---

# Create an administrator on first start

A standalone server (availability mode `none`) that starts writable on a data directory with no administrator creates
one named `admin` with a random password. The password goes to `<data-dir>/initial-admin-password` with mode `0600`, and
the startup banner and log show only that path. Store the password and delete the file. `peryx bootstrap-administrator`
still works when run before the first start.
