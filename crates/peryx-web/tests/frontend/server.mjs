import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdtempSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

export async function startPeryx({
  configText,
  port,
  readyPort,
  repo,
  prepare = async () => {},
  close = [],
}) {
  const metadata = spawnSync(
    "cargo",
    ["metadata", "--format-version=1", "--no-deps"],
    { cwd: repo, encoding: "utf8" },
  );
  if (metadata.status !== 0) throw new Error(metadata.stderr);
  const target = resolve(
    repo,
    process.env.CARGO_TARGET_DIR ?? JSON.parse(metadata.stdout).target_directory,
  );
  const binary =
    process.env.PERYX_FRONTEND_BINARY ??
    ["debug", "release"]
      .map((profile) => join(target, profile, "peryx"))
      .find(existsSync);
  if (!binary)
    throw new Error(
      "build the server and web bundle first: cargo leptos build",
    );

  const data = mkdtempSync(join(tmpdir(), "peryx-frontend-"));
  const config = join(data, "peryx.toml");
  writeFileSync(config, configText);
  const bootstrap = spawnSync(
    binary,
    [
      "bootstrap-administrator",
      "administrator",
      "--password-stdin",
      "--data-dir",
      data,
      "--config",
      config,
    ],
    { cwd: repo, encoding: "utf8", input: "browser-admin-secret\n" },
  );
  if (bootstrap.status !== 0) throw new Error(bootstrap.stderr);

  const peryx = spawn(
    binary,
    ["serve", "--port", String(port), "--data-dir", data, "--config", config],
    {
      cwd: join(repo, ".tox", "frontend"),
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const ready = createServer((_request, response) => {
    response.writeHead(200, { "content-type": "text/plain" });
    response.end("ready");
  });
  let stopping = false;
  let cleaned = false;
  let closed = false;
  const cleanup = () => {
    if (cleaned) return;
    cleaned = true;
    ready.close();
    for (const callback of close) callback();
  };
  const stop = (signal = "SIGTERM") => {
    if (stopping) return;
    stopping = true;
    cleanup();
    peryx.kill(signal);
    peryx.once("close", () => process.exit(0));
    setTimeout(() => {
      peryx.kill("SIGKILL");
      process.exit(1);
    }, 10_000).unref();
  };
  process.on("exit", () => {
    cleanup();
    peryx.kill();
  });
  for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"])
    process.on(signal, () => stop(signal));

  const started = Promise.withResolvers();
  let startupOutput = "";
  for (const [stream, output] of [
    [peryx.stdout, process.stdout],
    [peryx.stderr, process.stderr],
  ]) {
    stream.on("data", (chunk) => {
      output.write(chunk);
      startupOutput = `${startupOutput}${chunk}`.slice(-256);
      if (startupOutput.includes("peryx listening")) started.resolve();
    });
  }
  peryx.once("error", started.reject);
  peryx.once("close", (code) => {
    closed = true;
    started.reject(new Error(`peryx exited before startup with status ${code}`));
  });
  const startupTimeout = setTimeout(
    () => started.reject(new Error("peryx startup timed out")),
    60_000,
  );
  startupTimeout.unref();
  try {
    await started.promise.finally(() => clearTimeout(startupTimeout));
    await prepare({ base: `http://127.0.0.1:${port}`, data });
    await new Promise((resolve, reject) => {
      ready.once("error", reject);
      ready.listen(readyPort, "127.0.0.1", resolve);
    });
  } catch (error) {
    cleanup();
    if (!closed) {
      peryx.kill("SIGKILL");
      await once(peryx, "close");
    }
    throw error;
  }
}
