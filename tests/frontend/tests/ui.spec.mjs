// Functional tests of the hydrated web UI: every reactive feature is driven the way a person would
// drive it, against a real velodex with a real uploaded package.
import { expect, test } from "@playwright/test";

const PROJECT_URL = "/browse?index=root%2Fpypi&project=veloxdemo";
const TOKEN = "playwright-secret";

/// Navigate and wait for the wasm bundle to hydrate, so clicks hit live handlers.
async function goto(page, url) {
  await page.goto(url);
  await page.waitForSelector("body[data-hydrated]");
}

test("dashboard shows identity, counters, and the topology", async ({ page }) => {
  await goto(page, "/");
  await expect(page.locator(".stat-row .stat")).toHaveCount(4);
  // The overlay folds its member indexes into one card with an ordered layer stack.
  await expect(page.locator(".index-grid .card")).toHaveCount(1);
  const overlay = page.locator(".card", { hasText: "root/pypi" });
  await expect(overlay.locator(".badge.kind-overlay")).toBeVisible();
  await expect(overlay.locator(".layer")).toHaveCount(2);
  await expect(overlay.locator(".layer").first()).toContainText("local");
  await expect(overlay.locator(".layer").first()).toContainText("uploads land here");
  await expect(overlay.locator(".layer-hint")).toContainText("first file match wins");
});

test("admin status is read-only and tolerates failed stats fetches", async ({ page }) => {
  await page.route("**/+stats**", (route) => route.fulfill({ status: 503, body: "{}" }));
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Status" }).click();

  await expect(page).toHaveURL(/\/admin\/status$/);
  await expect(page.locator(".ops-title")).toContainText("read-only");
  await expect(page.locator(".ops-table").first()).toContainText("root/pypi");
  await expect(page.locator(".ops-table").first()).toContainText("redacted");
  await expect(page.locator(".ops-table", { hasText: "veloxdemo-1.0.0" })).toBeVisible();
  await expect(page.locator(".ops-table").first()).not.toContainText(TOKEN);
  await expect(page.locator(".dim", { hasText: "No usage recorded yet." })).toBeVisible();
  await expect(page.locator(".token")).toHaveCount(0);
  await expect(page.locator(".admin-table")).toHaveCount(0);
});

test("theme toggle switches and survives a reload", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".theme-toggle").click();
  const forced = await page.evaluate(() => document.documentElement.dataset.theme);
  expect(["light", "dark"]).toContain(forced);
  await page.reload();
  await expect.poll(() => page.evaluate(() => document.documentElement.dataset.theme)).toBe(forced);
});

test("project list filters reactively", async ({ page }) => {
  await goto(page, "/browse?index=root%2Fpypi");
  const entry = page.locator(".project-list li", { hasText: "veloxdemo" });
  await expect(entry).toBeVisible();
  await page.locator(".search").fill("zzz");
  await expect(entry).toHaveCount(0);
  await page.locator(".search").fill("velox");
  await expect(entry).toBeVisible();
});

test("project page renders pypi.org-style metadata", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await expect(page.locator(".project-head h1")).toContainText("veloxdemo");
  await expect(page.locator(".summary")).toContainText("A demonstration package");
  await expect(page.locator(".install code")).toContainText("uv pip install");
  // The markdown description renders as HTML, with inline emphasis intact.
  await expect(page.locator(".description h2")).toContainText("Features");
  await expect(page.locator(".description strong")).toContainText("velox");
  // The grouped side panel.
  const side = page.locator(".project-side");
  await expect(side).toContainText("MIT");
  await expect(side).toContainText("requests>=2");
  await expect(side.locator(".classifier-group", { hasText: "Development Status" })).toBeVisible();
  await expect(side.locator(".links-list a", { hasText: "Documentation" })).toBeVisible();
  // The file table shows size, hash, and the metadata badge.
  const row = page.locator("table.files tbody tr");
  await expect(row.locator(".badge.meta-badge")).toBeVisible();
  await expect(row).toContainText("1.2 kB");
});

test("archive browser lists members and shows file content", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await page.locator("a.inspect").click();
  await expect(page.locator(".archive-tree .archive-name.folder", { hasText: "veloxdemo-1.0.0.dist-info" })).toBeVisible();
  const metadataRow = page.locator(".archive-tree a.kind-text", { hasText: "METADATA" });
  await expect(metadataRow).toBeVisible();
  await metadataRow.click();
  await expect(page.locator(".member-content")).toContainText("Metadata-Version: 2.1");
  await page.locator("a", { hasText: "back to archive" }).click();
  await expect(page.locator(".archive-tree a.kind-text", { hasText: "__init__.py" })).toBeVisible();
});

test("admin panel yanks and un-yanks with the upload token", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await page.locator(".admin summary").click();
  await page.locator(".token").fill(TOKEN);

  await page.locator(".admin-table button", { hasText: /^yank$/ }).click();
  await expect(page.locator(".outcome")).toContainText("200");
  await expect(page.locator("table.files .badge.yanked-badge")).toBeVisible();

  await page.locator(".admin-table button", { hasText: "un-yank" }).click();
  await expect(page.locator("table.files .badge.yanked-badge")).toHaveCount(0);
});

test("wrong token surfaces the auth failure", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await page.locator(".admin summary").click();
  await page.locator(".token").fill("wrong");
  await page.locator(".admin-table button", { hasText: /^yank$/ }).click();
  await expect(page.locator(".outcome")).toContainText("401");
});

test("usage stats drill from index to project to file", async ({ page }) => {
  // Generate traffic the counters can show: a page view and a file download.
  const detail = await page.request.get("/root/pypi/simple/veloxdemo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  const files = (await detail.json()).files;
  await page.request.get(files[0].url);

  await goto(page, "/");
  const overlay = page.locator(".card", { hasText: "root/pypi" });
  await expect(overlay.locator(".card-usage")).toContainText("downloads");
  await overlay.locator(".card-usage a", { hasText: "usage" }).click();

  await expect(page.locator(".breadcrumb")).toContainText("root/pypi");
  await expect.poll(async () => page.locator(".stats-table tbody tr").count()).toBeGreaterThan(0);
  await page.locator(".stats-table a", { hasText: "veloxdemo" }).click();

  await expect(page.locator(".breadcrumb")).toContainText("veloxdemo");
  await expect(page.locator(".stats-table tbody tr", { hasText: "veloxdemo-1.0.0" }).first()).toBeVisible();
});
