// Capture the documentation screenshots: each page in both themes, against a live velodex.
// Usage: node capture-screens.mjs [base-url]  (default http://127.0.0.1:4499)
import { chromium } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const base = process.argv[2] ?? "http://127.0.0.1:4499";
const outDir = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "site", "static", "screens");

const pages = [
  { name: "dashboard", path: "/", height: 560 },
  { name: "stats-index", path: "/stats?index=root%2Fpypi", height: 720 },
  { name: "stats-project", path: "/stats?index=root%2Fpypi&project=requests", height: 640 },
  { name: "project", path: "/browse?index=root%2Fpypi&project=veloxdemo", height: 900 },
];

const browser = await chromium.launch();
for (const theme of ["light", "dark"]) {
  const context = await browser.newContext({ viewport: { width: 1360, height: 900 }, colorScheme: theme });
  await context.addInitScript((value) => localStorage.setItem("theme", value), theme);
  const page = await context.newPage();
  for (const shot of pages) {
    await page.goto(base + shot.path, { waitUntil: "networkidle" });
    await page.waitForTimeout(400);
    await page.screenshot({
      path: join(outDir, `${shot.name}-${theme}.png`),
      clip: { x: 0, y: 0, width: 1360, height: shot.height },
    });
    console.log(`${shot.name}-${theme}.png`);
  }
  await context.close();
}
await browser.close();
