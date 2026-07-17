import { defineConfig } from "@playwright/test";

const port = process.env.PERYX_FRONTEND_PORT ?? "4455";

// The web server script builds a temp data dir, starts the peryx binary with an upload token, and
// uploads the fixture wheel, so every run starts from the same state.
export default defineConfig({
  testDir: "tests",
  fullyParallel: true,
  retries: process.env.CI ? 2 : 0,
  use: {
    baseURL: `http://127.0.0.1:${port}`,
  },
  webServer: {
    command: "node serve.mjs",
    url: `http://127.0.0.1:${port}/+status`,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe",
    timeout: 60_000,
  },
});
