import { chromium } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { BROWSER_PATH, verifyBrowser } from "./test-support.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:4455";
const outDir = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "site",
  "static",
  "screens",
);

const pages = [
  { name: "dashboard", path: "/", height: 1240 },
  { name: "status", path: "/admin/status", height: 720 },
];

const browser = await chromium.launch({ executablePath: BROWSER_PATH });
await verifyBrowser(browser);
for (const theme of ["light", "dark"]) {
  const context = await browser.newContext({
    viewport: { width: 1360, height: 1280 },
    colorScheme: theme,
  });
  await context.addInitScript(
    (value) => localStorage.setItem("theme", value),
    theme,
  );
  const page = await context.newPage();
  for (const shot of pages) {
    await page.goto(base + shot.path, { waitUntil: "networkidle" });
    await page.evaluate(async () => {
      await document.fonts.ready;
      await new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      );
    });
    await page.screenshot({
      path: join(outDir, `${shot.name}-${theme}.png`),
      clip: { x: 0, y: 0, width: 1360, height: shot.height },
    });
    console.log(`${shot.name}-${theme}.png`);
  }
  await context.close();
}
await browser.close();
