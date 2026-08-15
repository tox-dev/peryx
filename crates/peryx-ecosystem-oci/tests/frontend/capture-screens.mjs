import { chromium } from "@playwright/test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const base = process.argv[2] ?? "http://127.0.0.1:4457";
const outDir = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "..",
  "site",
  "static",
  "screens",
);
const browser = await chromium.launch();
try {
  for (const theme of ["light", "dark"]) {
    const context = await browser.newContext({
      viewport: { width: 1360, height: 1280 },
      colorScheme: theme,
    });
    try {
      await context.addInitScript(
        (value) => localStorage.setItem("theme", value),
        theme,
      );
      const page = await context.newPage();
      await page.goto(`${base}/browse?index=images&project=app&ref=1.0`, {
        waitUntil: "networkidle",
      });
      await page.evaluate(async () => {
        await document.fonts.ready;
        await new Promise(requestAnimationFrame);
        await new Promise(requestAnimationFrame);
      });
      await page.screenshot({
        path: join(outDir, `oci-manifest-${theme}.png`),
        clip: { x: 0, y: 0, width: 1360, height: 550 },
      });
    } finally {
      await context.close();
    }
  }
} finally {
  await browser.close();
}
