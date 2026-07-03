// Start a velodex configured with an upload token, then upload the fixture wheel so the UI has a
// metadata-rich package to show. Playwright polls /+status for readiness.
import { spawn } from "node:child_process";
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
mirror = "https://pypi.org/simple/"

[[index]]
name = "local"
upload_token = "playwright-secret"

[[index]]
name = "root/pypi"
layers = ["local", "pypi"]
upload = "local"
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
console.log("velodex ready with the fixture uploaded");
