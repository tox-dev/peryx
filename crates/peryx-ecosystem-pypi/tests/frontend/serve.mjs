import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { startPeryx } from "../../../peryx-web/tests/frontend/server.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const port = Number(process.env.PERYX_PYPI_FRONTEND_PORT ?? 4456);
const upstreamPort = Number(process.env.PERYX_PYPI_UPSTREAM_PORT ?? 4454);
const upstreamBase = `http://127.0.0.1:${upstreamPort}`;
const wheel = readFileSync(
  join(here, "..", "fixtures", "veloxdemo-1.0.0-py3-none-any.whl"),
);

function file(filename) {
  const digest = createHash("sha256").update(filename).digest("hex");
  return {
    filename,
    url: `${upstreamBase}/files/${digest}/${filename}`,
    hashes: { sha256: digest },
    size: filename.length,
    "upload-time": "2026-01-01T00:00:00Z",
    yanked: false,
  };
}

const largeVersions = Array.from(
  { length: 100 },
  (_, version) => `${version}.0`,
);
const simplePages = new Map([
  [
    "/simple/",
    {
      meta: { "api-version": "1.1" },
      projects: [{ name: "large-demo" }, { name: "veloxdemo" }],
    },
  ],
  [
    "/simple/veloxdemo/",
    {
      meta: { "api-version": "1.1" },
      name: "veloxdemo",
      versions: ["0.9"],
      files: [
        {
          ...file("veloxdemo-0.9-py3-none-any.whl"),
          provenance: `${upstreamBase}/files/veloxdemo-0.9-py3-none-any.whl.provenance`,
        },
      ],
    },
  ],
  [
    "/simple/large-demo/",
    {
      meta: { "api-version": "1.1" },
      name: "large-demo",
      versions: largeVersions,
      files: largeVersions.flatMap((version) =>
        Array.from({ length: 20 }, (_, build) =>
          file(
            `large_demo-${version}-${String(build).padStart(3, "0")}-py3-none-any.whl`,
          ),
        ),
      ),
    },
  ],
]);
const upstream = createServer((request, response) => {
  const path = new URL(request.url, upstreamBase).pathname;
  if (simplePages.has(path)) {
    response.writeHead(200, {
      "content-type": "application/vnd.pypi.simple.v1+json",
    });
    response.end(JSON.stringify(simplePages.get(path)));
  } else if (path.startsWith("/files/")) {
    response.writeHead(200, { "content-type": "application/octet-stream" });
    response.end(decodeURIComponent(path.split("/").at(-1)));
  } else {
    response.writeHead(404);
    response.end("not found");
  }
});
await new Promise((resolve, reject) => {
  upstream.once("error", reject);
  upstream.listen(upstreamPort, "127.0.0.1", resolve);
});

function attestationsField(filename) {
  const sha256 = createHash("sha256").update(wheel).digest("hex");
  const statement = Buffer.from(
    JSON.stringify({
      _type: "https://in-toto.io/Statement/v1",
      subject: [{ name: filename, digest: { sha256 } }],
      predicateType: "https://docs.pypi.org/attestations/publish/v1",
      predicate: {},
    }),
  ).toString("base64");
  return JSON.stringify([
    {
      version: 1,
      verification_material: { certificate: "Zm9v", transparency_entries: [] },
      envelope: { statement, signature: "YmFy" },
    },
  ]);
}

await startPeryx({
  configText: `[[index]]
name = "pypi"
ecosystem = "pypi"

[[index.upstream]]
name = "fixture"
url = "${upstreamBase}/simple/"

[[index]]
name = "hosted"
ecosystem = "pypi"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index]]
name = "internal"
ecosystem = "pypi"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index.access_token]]
name = "reader"
secret = "playwright-reader"
actions = ["read"]

[[index]]
name = "limited"
ecosystem = "pypi"
hosted = true

[index.policy]
max_file_size_bytes = 512

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index]]
name = "zz-browser-upload"
ecosystem = "pypi"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index]]
name = "root/pypi"
ecosystem = "pypi"
layers = ["hosted", "pypi"]
write_target = "hosted"
`,
  port,
  readyPort: Number(process.env.PERYX_PYPI_READY_PORT ?? 5456),
  repo: join(here, "..", "..", "..", ".."),
  close: [() => upstream.close()],
  prepare: async ({ base }) => {
    const form = new FormData();
    form.set(":action", "file_upload");
    form.set("name", "veloxdemo");
    form.set("version", "1.0.0");
    form.set("filetype", "bdist_wheel");
    form.set(
      "attestations",
      attestationsField("veloxdemo-1.0.0-py3-none-any.whl"),
    );
    form.set("content", new Blob([wheel]), "veloxdemo-1.0.0-py3-none-any.whl");
    const response = await fetch(`${base}/root/pypi/`, {
      method: "POST",
      headers: {
        authorization: `Basic ${Buffer.from("__token__:playwright-secret").toString("base64")}`,
      },
      body: form,
    });
    if (!response.ok)
      throw new Error(
        `upload rejected: ${response.status} ${await response.text()}`,
      );
    const search = await fetch(`${base}/+search?q=veloxdemo&page_size=1`);
    const body = await search.text();
    if (!search.ok || !body.includes("veloxdemo"))
      throw new Error(
        `search index did not publish the fixture: ${search.status} ${body}`,
      );
  },
});
