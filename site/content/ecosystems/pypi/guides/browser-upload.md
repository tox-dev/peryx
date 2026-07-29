+++
title = "Upload from a browser"
description = "Publish one wheel or source distribution through the web UI with the same validation and storage path as twine and uv."
weight = 6
+++

Open `/upload` and select an upload-enabled PyPI index. Enter its upload token, then choose one `.whl` or `.tar.gz`
file. The page reports transfer progress and the server's rejection rule beside the filename. The token remains in the
page's password field and request memory until reload; peryx writes no browser storage or URL parameter.

The selected index needs a hosted upload target whose token grants write access to the package. A hosted index receives
the file itself. A writable virtual index receives the file through its configured `upload` layer, under the same route
used by `twine upload` and `uv publish`. A cached index and a virtual index without an upload target do not appear in
the selector.

Peryx sends the browser's `File` through `FormData`. The browser streams that file to the existing legacy upload
endpoint; the page does not read the artifact into WebAssembly memory. Peryx derives the legacy form identity with its
package-filename parser, then runs the multipart staging and package validation used for twine and uv. Both paths use
the same metadata transaction and blob commit.

The index's `max_file_size_bytes` policy applies while the multipart body streams. A rejected or cancelled transfer
removes its unpublished stage, and any failure before the commit leaves no package release in the index. A storage
failure produces a bounded message in the page; it does not return a temporary path or storage error detail to the
operator's browser.

## CSRF check

The page sends `X-Peryx-CSRF` with the browser's current origin. Peryx requires that value to match the `Origin` header
and target host, and rejects a request whose `Sec-Fetch-Site` header is not `same-origin`. A cross-origin HTML form
cannot set the custom header. Standard publishing clients send no browser `Origin` header, so their upload requests keep
the existing authentication behavior.

Do not place the web UI behind a proxy that rewrites `Host` away from the public origin. Preserve the incoming host so
peryx can compare it with the browser's origin. A rejected check returns `browser upload rejected` and publishes
nothing.

## Cancellation

`Cancel` aborts the active `XMLHttpRequest`. If the server is still reading the multipart body, the read fails and peryx
deletes the staged bytes. The page disables the button after it receives the server response because the commit has
finished by then.

## Differences from twine and uv

The browser page handles one wheel or modern `.tar.gz` source distribution per request. Use twine or uv for release
automation and multiple files; those clients also support `.zip` source distributions and uploaded attestations. They
provide the legacy form identity and optional digests themselves. The browser path derives the identity from the
filename and lets peryx compute the content digest during staging. Both paths finish in the same validator and
transaction.
