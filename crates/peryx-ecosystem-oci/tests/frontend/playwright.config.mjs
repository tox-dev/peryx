import { defineConfig } from "@playwright/test";

import { BROWSER_PATH } from "./test-support.mjs";

const port = process.env.PERYX_OCI_FRONTEND_PORT ?? "4457";
const readyPort = process.env.PERYX_OCI_READY_PORT ?? "5457";

export default defineConfig({
  testDir: "tests",
  outputDir: "../../../../.tox/frontend/oci/test-results",
  reporter: [
    ["line"],
    ["html", { outputFolder: "../../../../.tox/frontend/oci/report", open: "never" }],
  ],
  projects: [{ name: "oci" }],
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
