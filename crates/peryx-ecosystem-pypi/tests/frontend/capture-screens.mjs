import { chromium } from "@playwright/test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const base = process.argv[2] ?? "http://127.0.0.1:4456";
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
const wheel = "veloxdemo-1.0.0-py3-none-any.whl";
const digest =
  "ab46ad722f3d0f9a9b655760ef0aa83233554531c5c02b722e84c658e0e462ec";
for (let round = 0; round < 3; round += 1) {
  await fetch(`${base}/root/pypi/simple/veloxdemo/`);
  await fetch(`${base}/root/pypi/files/${digest}/${wheel}`);
  await fetch(`${base}/root/pypi/files/${digest}/${wheel}.metadata`);
}
await fetch(`${base}/root/pypi/simple/`);

const browser = await chromium.launch();
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
  for (const shot of [
    { name: "stats-index", path: "/stats?index=root%2Fpypi", height: 455 },
    {
      name: "stats-project",
      path: "/stats?index=root%2Fpypi&project=veloxdemo",
      height: 495,
    },
    {
      name: "project",
      path: "/browse?index=root%2Fpypi&project=veloxdemo",
      height: 900,
    },
  ]) {
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
  }
  await context.close();
}
await browser.close();
