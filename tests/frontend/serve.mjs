// Start a velodex configured with an upload token, then upload the fixture wheel so the UI has a
// metadata-rich package to show. Playwright polls /+status for readiness.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const binary = ["release", "debug"].map((profile) => join(repo, "target", profile, "velodex")).find(existsSync);
if (!binary) {
  console.error("build velodex first: cargo build -p velodex");
  process.exit(1);
}

const data = mkdtempSync(join(tmpdir(), "velodex-frontend-"));
const config = join(data, "velodex.toml");
writeFileSync(
  config,
  `[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]]
name = "hosted"
upload_token = "playwright-secret"

[[index]]
name = "internal"
upload_token = "playwright-secret"

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"

[[index]]
name = "images"
ecosystem = "oci"
upload_token = "playwright-secret"
`,
);

const velodex = spawn(binary, ["serve", "--port", "4455", "--data-dir", data, "--config", config], {
  cwd: repo, // the /pkg asset route serves ui/pkg relative to the working directory
  stdio: "inherit",
});
process.on("exit", () => velodex.kill());
for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) {
  // A plain signal skips the exit handler, which leaks velodex on the port; forward and quit.
  process.on(signal, () => {
    velodex.kill();
    process.exit(0);
  });
}

const wheel = readFileSync(join(here, "fixtures", "veloxdemo-1.0.0-py3-none-any.whl"));
for (let attempt = 0; attempt < 100; attempt += 1) {
  try {
    const form = new FormData();
    form.set(":action", "file_upload");
    form.set("name", "veloxdemo");
    form.set("version", "1.0.0");
    form.set("filetype", "bdist_wheel");
    form.set("content", new Blob([wheel]), "veloxdemo-1.0.0-py3-none-any.whl");
    const response = await fetch("http://127.0.0.1:4455/root/pypi/", {
      method: "POST",
      headers: { authorization: `Basic ${Buffer.from("__token__:playwright-secret").toString("base64")}` },
      body: form,
    });
    if (response.ok) break;
    console.error(`upload rejected: ${response.status} ${await response.text()}`);
    process.exit(1);
  } catch {
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
const blobResponse = await fetch(`http://127.0.0.1:4455/v2/images/app/blobs/uploads/?digest=${layerDigest}`, {
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
const configResponse = await fetch(`http://127.0.0.1:4455/v2/images/app/blobs/uploads/?digest=${configDigest}`, {
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
const manifestResponse = await fetch("http://127.0.0.1:4455/v2/images/app/manifests/1.0", {
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

console.log("velodex ready with the fixture uploaded");
