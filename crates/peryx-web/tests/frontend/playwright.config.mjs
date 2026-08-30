import { defineConfig } from "@playwright/test";

import { BROWSER_PATH } from "./test-support.mjs";

const port = process.env.PERYX_FRONTEND_PORT ?? "4455";
const readyPort = process.env.PERYX_READY_PORT ?? "5455";

export default defineConfig({
  testDir: "tests",
  outputDir: "../../../../.tox/frontend/shared/test-results",
  reporter: [
    ["line"],
    ["html", { outputFolder: "../../../../.tox/frontend/shared/report", open: "never" }],
  ],
  projects: [{ name: "shared" }],
  fullyParallel: true,
  retries: 0,
  workers: process.env.CI ? 1 : undefined,
  timeout: 60_000,
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    launchOptions: { executablePath: BROWSER_PATH },
  },
  webServer: {
    command: "node serve.mjs",
    url: `http://127.0.0.1:${readyPort}/`,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe",
    timeout: 120_000,
    gracefulShutdown: { signal: "SIGINT", timeout: 15_000 },
  },
});
