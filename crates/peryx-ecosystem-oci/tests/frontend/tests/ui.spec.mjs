import { expect, test } from "@playwright/test";

import {
  collectWasmCoverage,
  goto,
  verifyBrowser,
} from "../test-support.mjs";

const HOST = `127.0.0.1:${process.env.PERYX_OCI_FRONTEND_PORT ?? 4457}`;

collectWasmCoverage(test);
test.beforeAll(async ({ browser }) => verifyBrowser(browser));

test("dashboard advertises the OCI registry endpoint", async ({ page }) => {
  await goto(page, "/");
  const images = page.locator(".card", { hasText: "images" });
  await expect(images.locator(".badge.ecosystem-oci")).toBeVisible();
  await expect(images).toContainText("/v2/images/");
});

test("OCI browse lists tags and opens a manifest", async ({ page }) => {
  await goto(page, "/browse?index=images&project=app");
  await expect(page.locator(".page")).toContainText("1.0");
  await page.getByRole("link", { name: "1.0" }).click();
  await expect(page).toHaveURL(/ref=1\.0/);
  await expect(page.locator(".page")).toContainText("Layers");
  await expect(page.locator(".browse-properties")).toContainText(
    /Config\s*sha256:/,
  );
  await expect(page.locator(".page")).toContainText(
    "application/vnd.oci.image.layer.v1.tar",
  );
  await expect(page.locator(".install code")).toContainText(
    `docker pull ${HOST}/images/app:1.0`,
  );
});

test("OCI browse lists layer files and previews text", async ({
  page,
}) => {
  await goto(page, "/browse?index=images&project=app&ref=1.0");
  await page.getByRole("link", { name: "contents" }).click();
  await expect(page).toHaveURL(/layer=/);
  await expect(page.locator(".page")).toContainText("etc/app.conf");
  await expect(page.locator(".page")).toContainText("bin/app");
  await page.getByRole("link", { name: "etc/app.conf" }).click();
  await expect(page.locator(".page")).toContainText("debug = true");
});
