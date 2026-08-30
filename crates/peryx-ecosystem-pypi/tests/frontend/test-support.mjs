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

export const ADMIN_AUTH = `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`;
const ERRORS = new WeakMap();

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
    const digest = createHash("sha256").update(identity).digest("hex");
    writeFileSync(
      join(process.env.PERYX_WASM_PROFRAW, `${digest}.profraw`),
      Buffer.from(profile),
    );
  });
}

export async function goto(page, url) {
  browserErrors(page);
  await page.goto(url);
  await page.waitForSelector("body[data-hydrated]");
}

export function browserErrors(page) {
  if (ERRORS.has(page)) return ERRORS.get(page);
  const errors = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      errors.push(`console error: ${message.text()}`);
      console.error(errors.at(-1));
    }
  });
  page.on("pageerror", (error) => {
    errors.push(`browser error: ${error.stack ?? error.message}`);
    console.error(errors.at(-1));
  });
  ERRORS.set(page, errors);
  return errors;
}

export async function operatorPage(subject) {
  const page = "newPage" in subject ? await subject.newPage() : subject;
  await page.setExtraHTTPHeaders({ authorization: ADMIN_AUTH });
  return page;
}

export async function openUpload(page, repository, token, files) {
  await page.goto(`/upload?index=${encodeURIComponent(repository)}`);
  await page.locator("#repository").selectOption(`/${repository}/`);
  await page.locator("#token").fill(token);
  await page.locator("#artifact").setInputFiles(files);
}
