import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { expect } from "@playwright/test";

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

export function boundaryCases(
  test,
  {
    title,
    cases,
    setupRoute,
    navigate,
    action = async () => {},
    createPage,
    closePage = false,
  },
) {
  for (const boundary of cases) {
    const run = async (page) => {
      try {
        await setupRoute(page, boundary);
        await navigate(page);
        await action(page);
        const alert = page.getByRole("alert");
        await expect(alert).toHaveText(boundary.expectedAlert);
        if (boundary.excludedText)
          await expect(alert).not.toContainText(boundary.excludedText);
      } finally {
        if (closePage) await page.context().close();
      }
    };
    if (createPage) {
      test(`${title} reports ${boundary.label}`, async ({ browser, page }) =>
        run(await createPage({ browser, page })),
      );
    } else {
      test(`${title} reports ${boundary.label}`, async ({ page }) => run(page));
    }
  }
}

export async function goto(page, url) {
  page.on("pageerror", (error) =>
    console.error(`browser error: ${error.stack || error.message || String(error)}`),
  );
  await page.goto(url, { waitUntil: "commit" });
  await page.waitForSelector("body[data-hydrated]");
}

export async function operatorPage(subject) {
  const page = "newPage" in subject ? await subject.newPage() : subject;
  await page.setExtraHTTPHeaders({
    authorization: `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`,
  });
  return page;
}
