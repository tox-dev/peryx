---
default: minor
---

# Sign in to the web UI with a local password

The login page offers a name and password form for local accounts, such as the first administrator, whenever the server
can seal browser sessions. A standalone server without `auth.signing_key` generates a session key in
`<data-dir>/session-key` (mode `0600`) and reuses it across restarts; a configured key still wins, and the generated one
seals browser sessions only.
