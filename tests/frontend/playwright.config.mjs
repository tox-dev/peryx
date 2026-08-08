import { defineConfig } from "@playwright/test";

const port = process.env.PERYX_FRONTEND_PORT ?? "4455";
const readyPort = process.env.PERYX_READY_PORT ?? String(Number(port) + 1000);

// The web server script builds a temp data dir, starts the peryx binary with an upload token, and
// uploads the fixture wheel, so every run starts from the same state.
export default defineConfig({
  testDir: "tests",
  fullyParallel: true,
  retries: process.env.CI ? 2 : 0,
  // A busy CI runner compiles and instantiates the debug wasm bundle far slower than a dev box, so the
  // default 30s per-test budget can expire mid-hydration on an otherwise healthy run. Retries do not
  // rescue that: every attempt is equally starved. Give each test wall-clock headroom instead.
  timeout: 60_000,
  use: {
    baseURL: `http://127.0.0.1:${port}`,
  },
  webServer: {
    command: "node serve.mjs",
    // serve.mjs binds this only after it has uploaded every fixture, so tests never race an empty
    // backend. peryx's own /+status answers the moment it binds, long before the uploads finish, so
    // gating on it would start the suite against a package that does not exist yet.
    url: `http://127.0.0.1:${readyPort}/`,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe",
    timeout: 120_000,
    gracefulShutdown: { signal: "SIGINT", timeout: 15_000 },
  },
});
