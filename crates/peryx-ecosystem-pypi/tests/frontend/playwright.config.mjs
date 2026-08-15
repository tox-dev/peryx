import { defineConfig } from "@playwright/test";

const port = process.env.PERYX_PYPI_FRONTEND_PORT ?? "4456";
const readyPort = process.env.PERYX_PYPI_READY_PORT ?? "5456";

export default defineConfig({
  testDir: "tests",
  outputDir: "../../../../.tox/frontend/pypi/test-results",
  reporter: [
    ["line"],
    ["html", { outputFolder: "../../../../.tox/frontend/pypi/report", open: "never" }],
  ],
  projects: [{ name: "pypi" }],
  fullyParallel: true,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  timeout: 60_000,
  use: { baseURL: `http://127.0.0.1:${port}` },
  webServer: {
    command: "node serve.mjs",
    url: `http://127.0.0.1:${readyPort}/`,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe",
    timeout: 120_000,
    gracefulShutdown: { signal: "SIGINT", timeout: 15_000 },
  },
});
