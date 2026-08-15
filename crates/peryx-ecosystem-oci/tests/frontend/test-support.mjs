import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

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
