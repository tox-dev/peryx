import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

import { expect } from "@playwright/test";

const require = createRequire(import.meta.url);
const browserVersion = JSON.parse(
  readFileSync(
    join(dirname(require.resolve("playwright-core/package.json")), "browsers.json"),
  ),
).browsers.find(({ name }) => name === "chromium-headless-shell").browserVersion;

export const BROWSER_PATH = process.env.PERYX_PLAYWRIGHT_BROWSER_PATH;
if (!BROWSER_PATH) throw new Error("PERYX_PLAYWRIGHT_BROWSER_PATH is required");
if (process.env.PERYX_PLAYWRIGHT_BROWSER_VERSION !== browserVersion)
  throw new Error("Playwright and its browser revision differ");

export async function verifyBrowser(browser) {
  expect(await browser.version()).toBe(browserVersion);
}

export function collectWasmCoverage(test) {
  test.afterEach(async ({ page }, testInfo) => {
    if (
      !process.env.PERYX_WASM_PROFRAW ||
      page.isClosed() ||
      (await page.locator("body[data-hydrated]").count()) === 0
    )
      return;
    const profile = await page.evaluate(async () => {
      const module = await import("/pkg/peryx_web.js");
      return Array.from(module.capture_coverage());
    });
    const identity = `${testInfo.project.name}-${testInfo.workerIndex}-${testInfo.retry}-${testInfo.titlePath.join("-")}`;
    writeFileSync(
      join(
        process.env.PERYX_WASM_PROFRAW,
        `${createHash("sha256").update(identity).digest("hex")}.profraw`,
      ),
      Buffer.from(profile),
    );
  });
}

export async function goto(page, url) {
  page.on("console", (message) => {
    if (message.type() === "error") console.error(message.text());
  });
  page.on("pageerror", (error) =>
    console.error(`browser error: ${error.stack ?? error.message}`),
  );
  await page.goto(url);
  await page.waitForSelector("body[data-hydrated]");
}
