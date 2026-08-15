import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { startPeryx } from "../../../peryx-web/tests/frontend/server.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const port = Number(process.env.PERYX_OCI_FRONTEND_PORT ?? 4457);

function tarLayer(files) {
  const blocks = [];
  for (const [name, content] of files) {
    const data = Buffer.from(content);
    const header = Buffer.alloc(512);
    header.write(name, 0, "utf8");
    header.write("0000644\0", 100, "ascii");
    header.write("0000000\0", 108, "ascii");
    header.write("0000000\0", 116, "ascii");
    header.write(
      `${data.length.toString(8).padStart(11, "0")}\0`,
      124,
      "ascii",
    );
    header.write("00000000000\0", 136, "ascii");
    header.write("        ", 148, "ascii");
    header.write("0", 156, "ascii");
    header.write("ustar\x0000", 257, "ascii");
    let checksum = 0;
    for (const byte of header) checksum += byte;
    header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, "ascii");
    blocks.push(header);
    const body = Buffer.alloc(Math.ceil(data.length / 512) * 512);
    data.copy(body);
    blocks.push(body);
  }
  blocks.push(Buffer.alloc(1024));
  return Buffer.concat(blocks);
}

await startPeryx({
  configText: `[[index]]
name = "images"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "playwright-secret"
actions = ["write", "delete"]
`,
  port,
  readyPort: Number(process.env.PERYX_OCI_READY_PORT ?? 5457),
  repo: join(here, "..", "..", "..", ".."),
  prepare: async ({ base }) => {
    const authorization = `Basic ${Buffer.from("_:playwright-secret").toString("base64")}`;
    const layer = tarLayer([
      ["etc/app.conf", "debug = true\nport = 8080\n"],
      ["bin/app", Buffer.from([0x7f, 0x45, 0x4c, 0x46])],
    ]);
    const layerDigest = `sha256:${createHash("sha256").update(layer).digest("hex")}`;
    const layerResponse = await fetch(
      `${base}/v2/images/app/blobs/uploads/?digest=${layerDigest}`,
      {
        method: "POST",
        headers: { authorization },
        body: layer,
      },
    );
    if (!layerResponse.ok)
      throw new Error(`layer upload rejected: ${layerResponse.status}`);

    const imageConfig = Buffer.from(
      JSON.stringify({
        architecture: "amd64",
        os: "linux",
        rootfs: { type: "layers", diff_ids: [layerDigest] },
      }),
    );
    const configDigest = `sha256:${createHash("sha256").update(imageConfig).digest("hex")}`;
    const configResponse = await fetch(
      `${base}/v2/images/app/blobs/uploads/?digest=${configDigest}`,
      {
        method: "POST",
        headers: { authorization },
        body: imageConfig,
      },
    );
    if (!configResponse.ok)
      throw new Error(`config upload rejected: ${configResponse.status}`);

    const manifestResponse = await fetch(
      `${base}/v2/images/app/manifests/1.0`,
      {
        method: "PUT",
        headers: {
          authorization,
          "content-type": "application/vnd.oci.image.manifest.v1+json",
        },
        body: JSON.stringify({
          schemaVersion: 2,
          mediaType: "application/vnd.oci.image.manifest.v1+json",
          config: {
            mediaType: "application/vnd.oci.image.config.v1+json",
            digest: configDigest,
            size: imageConfig.length,
          },
          layers: [
            {
              mediaType: "application/vnd.oci.image.layer.v1.tar",
              digest: layerDigest,
              size: layer.length,
            },
          ],
        }),
      },
    );
    if (!manifestResponse.ok)
      throw new Error(`manifest push rejected: ${manifestResponse.status}`);
  },
});
