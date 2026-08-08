// Start a peryx configured with an upload token, then upload the fixture wheel so the UI has a
// metadata-rich package to show. Playwright polls /+status for readiness.
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const target = resolve(repo, process.env.CARGO_TARGET_DIR ?? "target");
const binary =
  process.env.PERYX_FRONTEND_BINARY ?? ["debug", "release"].map((profile) => join(target, profile, "peryx")).find(existsSync);
if (!binary) {
  console.error("build the server and web bundle first: cargo leptos build");
  process.exit(1);
}
const port = Number(process.env.PERYX_FRONTEND_PORT ?? 4455);
const upstreamPort = Number(process.env.PERYX_UPSTREAM_PORT ?? 4454);
const readyPort = Number(process.env.PERYX_READY_PORT ?? port + 1000);
const base = `http://127.0.0.1:${port}`;
const upstreamBase = `http://127.0.0.1:${upstreamPort}`;

const data = mkdtempSync(join(tmpdir(), "peryx-frontend-"));
const config = join(data, "peryx.toml");

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

const largeVersions = Array.from({ length: 100 }, (_, version) => `${version}.0`);
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
          file(`large_demo-${version}-${String(build).padStart(3, "0")}-py3-none-any.whl`),
        ),
      ),
    },
  ],
]);
const upstream = createServer((request, response) => {
  const path = new URL(request.url, upstreamBase).pathname;
  if (simplePages.has(path)) {
    response.writeHead(200, { "content-type": "application/vnd.pypi.simple.v1+json" });
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

writeFileSync(
  config,
  `[[index]]
name = "pypi"

[[index.upstream]]
name = "fixture"
url = "${upstreamBase}/simple/"

[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index]]
name = "internal"
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
hosted = true

[index.policy]
max_file_size_bytes = 512

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index]]
name = "zz-browser-upload"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[[index]]
name = "images"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]
`,
);

const bootstrap = spawnSync(
  binary,
  ["bootstrap-administrator", "administrator", "--password-stdin", "--data-dir", data, "--config", config],
  { cwd: repo, encoding: "utf8", input: "browser-admin-secret\n" },
);
if (bootstrap.status !== 0) {
  console.error(bootstrap.stderr);
  process.exit(1);
}

const peryx = spawn(binary, ["serve", "--port", port.toString(), "--data-dir", data, "--config", config], {
  cwd: repo, // the /pkg asset route serves ui/pkg relative to the working directory
  stdio: "inherit",
});
let ready;
process.on("exit", () => {
  peryx.kill();
  upstream.close();
  ready?.close();
});
let stopping = false;
for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) {
  process.on(signal, () => {
    if (stopping) return;
    stopping = true;
    peryx.once("close", () => {
      upstream.close();
      ready?.close();
      process.exit(0);
    });
    setTimeout(() => {
      peryx.kill("SIGKILL");
      process.exit(1);
    }, 10_000).unref();
  });
}

const wheel = readFileSync(join(here, "fixtures", "veloxdemo-1.0.0-py3-none-any.whl"));
// A PEP 740 attestation bound to the uploaded wheel, so its hosted file shows a summarized provenance
// panel. peryx stores and echoes the untrusted material; it verifies no signature.
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
for (let attempt = 0; attempt < 600; attempt += 1) {
  try {
    const form = new FormData();
    form.set(":action", "file_upload");
    form.set("name", "veloxdemo");
    form.set("version", "1.0.0");
    form.set("filetype", "bdist_wheel");
    form.set("attestations", attestationsField("veloxdemo-1.0.0-py3-none-any.whl"));
    form.set("content", new Blob([wheel]), "veloxdemo-1.0.0-py3-none-any.whl");
    const response = await fetch(`${base}/root/pypi/`, {
      method: "POST",
      headers: { authorization: `Basic ${Buffer.from("__token__:playwright-secret").toString("base64")}` },
      body: form,
    });
    if (response.ok) break;
    console.error(`upload rejected: ${response.status} ${await response.text()}`);
    process.exit(1);
  } catch (error) {
    if (attempt === 599) throw error;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}
// Build a real (uncompressed) tar layer so the layer browser has files to list and preview, upload it
// as a blob, then push a manifest that references it by its true digest.
function tarLayer(files) {
  const blocks = [];
  for (const [name, content] of files) {
    const data = Buffer.from(content);
    const header = Buffer.alloc(512);
    header.write(name, 0, "utf8");
    header.write("0000644\0", 100, "ascii");
    header.write("0000000\0", 108, "ascii");
    header.write("0000000\0", 116, "ascii");
    header.write(`${data.length.toString(8).padStart(11, "0")}\0`, 124, "ascii");
    header.write("00000000000\0", 136, "ascii");
    header.write("        ", 148, "ascii");
    header.write("0", 156, "ascii");
    header.write("ustar\x0000", 257, "ascii");
    let sum = 0;
    for (let i = 0; i < 512; i += 1) sum += header[i];
    header.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148, "ascii");
    blocks.push(header);
    const body = Buffer.alloc(Math.ceil(data.length / 512) * 512);
    data.copy(body);
    blocks.push(body);
  }
  blocks.push(Buffer.alloc(1024));
  return Buffer.concat(blocks);
}

const layer = tarLayer([
  ["etc/app.conf", "debug = true\nport = 8080\n"],
  ["bin/app", Buffer.from([0x7f, 0x45, 0x4c, 0x46])],
]);
const layerDigest = `sha256:${createHash("sha256").update(layer).digest("hex")}`;
const blobResponse = await fetch(`${base}/v2/images/app/blobs/uploads/?digest=${layerDigest}`, {
  method: "POST",
  headers: { authorization: `Basic ${Buffer.from("_:playwright-secret").toString("base64")}` },
  body: layer,
});
if (!blobResponse.ok) {
  console.error(`layer upload rejected: ${blobResponse.status} ${await blobResponse.text()}`);
  process.exit(1);
}
// The manifest must reference blobs the registry holds, so upload a real config blob too.
const imageConfig = Buffer.from(JSON.stringify({ architecture: "amd64", os: "linux", rootfs: { type: "layers", diff_ids: [layerDigest] } }));
const configDigest = `sha256:${createHash("sha256").update(imageConfig).digest("hex")}`;
const configResponse = await fetch(`${base}/v2/images/app/blobs/uploads/?digest=${configDigest}`, {
  method: "POST",
  headers: { authorization: `Basic ${Buffer.from("_:playwright-secret").toString("base64")}` },
  body: imageConfig,
});
if (!configResponse.ok) {
  console.error(`config upload rejected: ${configResponse.status} ${await configResponse.text()}`);
  process.exit(1);
}
const manifest = JSON.stringify({
  schemaVersion: 2,
  mediaType: "application/vnd.oci.image.manifest.v1+json",
  config: { mediaType: "application/vnd.oci.image.config.v1+json", digest: configDigest, size: imageConfig.length },
  layers: [{ mediaType: "application/vnd.oci.image.layer.v1.tar", digest: layerDigest, size: layer.length }],
});
const manifestResponse = await fetch(`${base}/v2/images/app/manifests/1.0`, {
  method: "PUT",
  headers: {
    authorization: `Basic ${Buffer.from("_:playwright-secret").toString("base64")}`,
    "content-type": "application/vnd.oci.image.manifest.v1+json",
  },
  body: manifest,
});
if (!manifestResponse.ok) {
  console.error(`manifest push rejected: ${manifestResponse.status} ${await manifestResponse.text()}`);
  process.exit(1);
}

let searchable = false;
for (let attempt = 0; attempt < 600; attempt += 1) {
  const response = await fetch(`${base}/+search?q=veloxdemo&page_size=1`);
  if (response.ok && (await response.text()).includes("veloxdemo")) {
    searchable = true;
    break;
  }
  await new Promise((resolve) => setTimeout(resolve, 100));
}
if (!searchable) {
  console.error("search index did not publish the fixture");
  process.exit(1);
}

// Bind the readiness port only now, once every fixture is in place. Playwright polls this before it
// runs any test; peryx's /+status answers far earlier, so gating on it would race the uploads above.
// A still-closed port reads as connection-refused, which Playwright treats as not-yet-ready.
ready = createServer((_request, response) => {
  response.writeHead(200, { "content-type": "text/plain" });
  response.end("ready");
});
await new Promise((resolve, reject) => {
  ready.once("error", reject);
  ready.listen(readyPort, "127.0.0.1", resolve);
});

console.log("peryx ready with the fixture uploaded");
