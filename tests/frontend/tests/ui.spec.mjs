// Functional tests of the hydrated web UI: every reactive feature is driven the way a person would
// drive it, against a real peryx with a real uploaded package.
import { expect, test } from "@playwright/test";

const PROJECT_URL = "/browse?index=root%2Fpypi&project=veloxdemo";
const TOKEN = "playwright-secret";
const HOST = `127.0.0.1:${process.env.PERYX_FRONTEND_PORT ?? "4455"}`;

/// Navigate and wait for the wasm bundle to hydrate, so clicks hit live handlers.
async function goto(page, url) {
  await page.goto(url);
  await page.waitForSelector("body[data-hydrated]");
}

test("dashboard shows identity, counters, and the topology", async ({ page }) => {
  await goto(page, "/");
  // Metrics are split into a global group and a per-ecosystem group, so a reader can tell the
  // instance-wide request count from PyPI-scoped counters like PEP 658 hits.
  const globalGroup = page.locator(".metrics-group", { hasText: "Global" });
  await expect(globalGroup.locator(".stat", { hasText: "requests served" })).toBeVisible();
  const pypiGroup = page.locator(".metrics-group", { has: page.locator(".badge.ecosystem-pypi") });
  await expect(pypiGroup.locator(".stat", { hasText: "listings served" })).toBeVisible();
  await expect(pypiGroup.locator(".stat", { hasText: "PEP 658 metadata hits" })).toBeVisible();
  await expect(globalGroup).not.toContainText("PEP 658");
  // The virtual index folds its member indexes into one card with an ordered layer stack.
  const virtualIndex = page.locator(".card", { hasText: "root/pypi" });
  await expect(virtualIndex.locator(".badge.kind-virtual")).toBeVisible();
  await expect(virtualIndex.locator(".layer")).toHaveCount(2);
  // The role trio is visible in the stack: a hosted store (the upload target) resolved over a cache.
  const hostedLayer = virtualIndex.locator(".layer").first();
  await expect(hostedLayer).toContainText("hosted");
  await expect(hostedLayer.locator(".badge.kind-hosted")).toBeVisible();
  await expect(hostedLayer).toContainText("uploads land here");
  await expect(virtualIndex.locator(".layer").nth(1).locator(".badge.kind-cached")).toBeVisible();
  await expect(virtualIndex.locator(".layer-hint")).toContainText("first file match wins");
  // A non-member index renders as a standalone card under its own heading with its role badge.
  await expect(page.locator("h2", { hasText: "Standalone indexes" })).toBeVisible();
  const standalone = page.locator(".card", { hasText: "internal" });
  await expect(standalone.locator(".badge.kind-hosted")).toBeVisible();
  await expect(standalone.locator(".badge.uploads")).toBeVisible();
  // An OCI index advertises its /v2/ registry endpoint, not a PyPI /simple/ URL.
  const images = page.locator(".card", { hasText: "images" });
  await expect(images.locator(".badge.ecosystem-oci")).toBeVisible();
  await expect(images).toContainText("/v2/images/");
  await expect(images).not.toContainText("/simple/");
});

test("header nav links reach each in-app route", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Search" }).click();
  await expect(page).toHaveURL(/\/search/);
  await page.locator(".nav-links a", { hasText: "Status" }).click();
  await expect(page).toHaveURL(/\/admin\/status$/);
  await page.locator(".nav-links a", { hasText: "Dashboard" }).click();
  await expect(page.locator(".card", { hasText: "root/pypi" })).toBeVisible();
  // External links carry the right targets without being followed.
  await expect(page.locator(".nav-links a", { hasText: "Docs" })).toHaveAttribute("href", /readthedocs/);
  await expect(page.locator(".nav-links a", { hasText: "GitHub" })).toHaveAttribute("href", /github\.com/);
});

test("header search suggests packages live and opens one", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".header-search input[name='q']").fill("velox");
  const suggestions = page.locator(".suggestions");
  await expect(suggestions).toBeVisible();
  const item = suggestions.locator("a.suggestion", { hasText: "veloxdemo" }).first();
  await expect(item).toBeVisible();
  await expect(item.locator("[class*='source-']")).toBeVisible();
  await expect(suggestions.locator("a.all-results")).toBeVisible();
  await item.click();
  await expect(page).toHaveURL(/project=veloxdemo/);
  await expect(page.locator(".project-head h1")).toContainText("veloxdemo");
});

test("search reports no matches and honors the provenance facet", async ({ page }) => {
  await goto(page, "/search?q=zzznotapackage");
  await expect(page.locator(".search-page")).toContainText("Nothing matched this search");
  await goto(page, "/search?q=large-demo&type=uploaded");
  await expect(page.locator(".search-page")).toContainText("Nothing matched this search");
  await expect(page.locator(".search-controls select[name='type']")).toHaveValue("uploaded");
});

test("search form submission navigates with the query", async ({ page }) => {
  await goto(page, "/search");
  await page.locator(".search-controls input[name='q']").fill("veloxdemo");
  await page.locator(".search-controls button[type='submit']").click();
  await expect(page).toHaveURL(/q=veloxdemo/);
  await expect(page.locator("table.search-results tbody tr", { hasText: "veloxdemo" }).first()).toBeVisible();
});

test("usage stats page lists indexes and drills into one", async ({ page }) => {
  // Seed a page view so the counters have a row to show.
  await page.request.get("/root/pypi/simple/veloxdemo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  await goto(page, "/stats");
  await expect(page.locator(".breadcrumb")).toContainText("usage");
  await expect.poll(async () => page.locator(".stats-table tbody tr").count()).toBeGreaterThan(0);
  await page.locator(".stats-table a", { hasText: "root/pypi" }).first().click();
  await expect(page).toHaveURL(/\/stats\?index=/);
  await expect(page.locator(".breadcrumb")).toContainText("root/pypi");
});

test("project server render keeps the relative install snippet", async ({ page }) => {
  const response = await page.request.get(PROJECT_URL);
  expect(await response.text()).toContain("uv pip install --index-url /root/pypi/simple/ veloxdemo");
});

test("project install snippet uses the browser origin when copied", async ({ page }) => {
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await goto(page, PROJECT_URL);
  const command = `uv pip install --index-url http://${HOST}/root/pypi/simple/ veloxdemo==1.0.0`;
  await expect(page.locator(".install code")).toHaveText(command);
  await page.locator(".install button.copy").click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(command);
});

test("project page downloads artifacts", async ({ page }) => {
  await goto(page, PROJECT_URL);
  // The file's own link resolves to the real content-addressed artifact.
  const href = await page
    .locator("table.files tbody tr td a", { hasText: /\.whl$/ })
    .first()
    .getAttribute("href");
  const download = await page.request.get(href);
  expect(download.status()).toBe(200);
});

test("unknown routes render the not-found fallback", async ({ page }) => {
  // An unmatched path never reaches the SPA shell — peryx answers with its own 404 body — so this
  // one skips the hydration wait the other tests rely on.
  const response = await page.goto("/does-not-exist");
  expect(response.status()).toBe(404);
  await expect(page.locator("body")).toContainText("not found");
});

test("admin table shows upstream and upload state per index", async ({ page }) => {
  await goto(page, "/admin/status");
  const table = page.locator(".ops-table").first();
  // The cached index reports a configured upstream; a hosted index shows an upload badge.
  await expect(table.locator(".badge.status-configured").first()).toBeVisible();
  await expect(table.locator("[class*='badge upload-']").first()).toBeVisible();
});

test("admin status is read-only and tolerates failed stats fetches", async ({ page }) => {
  await page.route("**/+stats**", (route) => route.fulfill({ status: 503, body: "{}" }));
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Status" }).click();

  await expect(page).toHaveURL(/\/admin\/status$/);
  await expect(page.locator(".ops-title")).toContainText("read-only");
  const topology = page.locator(".ops-table").first();
  await expect(topology).toContainText("root/pypi");
  await expect(topology).toContainText("redacted");
  // The topology table renders both axes: the pypi ecosystem and every role (cached/hosted/virtual).
  await expect(topology.locator(".badge.ecosystem-pypi").first()).toBeVisible();
  await expect(topology.locator(".badge.kind-cached")).toBeVisible();
  await expect(topology.locator(".badge.kind-hosted").first()).toBeVisible();
  await expect(topology.locator(".badge.kind-virtual")).toBeVisible();
  await expect(page.locator(".ops-table", { hasText: "veloxdemo-1.0.0" })).toBeVisible();
  await expect(page.locator(".ops-table").first()).not.toContainText(TOKEN);
  await expect(page.locator(".dim", { hasText: "No usage recorded yet." })).toBeVisible();
  await expect(page.locator(".token")).toHaveCount(0);
  await expect(page.locator(".admin-table")).toHaveCount(0);
});

test("every page sets the differentiated app favicon", async ({ page }) => {
  await goto(page, "/admin/status");
  await expect(page.locator("head link[rel='icon']")).toHaveAttribute("href", "/favicon.svg");
  const response = await page.request.get("/favicon.svg");
  expect(response.headers()["content-type"]).toContain("image/svg+xml");
  const svg = await response.text();
  // The peryx mark (no wordmark) with a green node: distinct from the docs site's blue node.
  expect(svg).toContain("512 512");
  expect(svg).toContain("#22C55E");
  expect(svg).not.toContain("#4F9BE0");
});

test("admin topology table fits the page and uses current vocabulary", async ({ page }) => {
  await goto(page, "/admin/status");
  // Renamed heading and the merged role x ecosystem "Type" column.
  await expect(page.locator(".ops-page h2", { hasText: "Indexes" })).toBeVisible();
  await expect(page.locator(".ops-page h2", { hasText: "Repositories" })).toHaveCount(0);
  await expect(page.locator(".ops-table th", { hasText: "Type" }).first()).toBeVisible();
  await expect(page.locator(".ops-table .ops-type").first().locator(".badge")).toHaveCount(2);
  // The wide data tables scroll inside their own container — the page body never scrolls sideways.
  const bodyScrollsSideways = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
  expect(bodyScrollsSideways).toBe(false);
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
  const row = page.locator("table.files tbody tr", { hasText: "veloxdemo-1.0.0" });
  await expect(row.locator(".badge.meta-badge")).toBeVisible();
  await expect(row).toContainText("1.2 kB");
});

test("project page groups hosted and upstream releases", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const groups = page.locator("section.release-files");
  await expect(groups).toHaveCount(2);
  await expect(groups.nth(0).getByRole("heading", { name: "Release 1.0.0" })).toBeVisible();
  await expect(groups.nth(1).getByRole("heading", { name: "Release 0.9" })).toBeVisible();
  await expect(groups.nth(0).locator("tr", { hasText: "veloxdemo-1.0.0" })).toHaveCount(1);
  const upstream = groups.nth(1).locator("tr", { hasText: "veloxdemo-0.9" });
  await expect(upstream).toHaveCount(1);
  await expect(upstream.getByTitle("Upstream source")).toHaveText("fixture");
});

test("release links retain filename filters and select one exact group", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await page.locator(".file-search").fill("0.9");
  const release = page.getByRole("navigation", { name: "Project releases" }).getByRole("link", { name: "0.9" });
  await release.click();

  await expect(page).toHaveURL(/version=0\.9&filename=0\.9/);
  await expect(release).toHaveAttribute("aria-current", "page");
  await expect(page.locator("section.release-files")).toHaveCount(1);
  await expect(page.locator("tr", { hasText: "veloxdemo-0.9" })).toHaveCount(1);
  await expect(page.locator("tr", { hasText: "veloxdemo-1.0.0" })).toHaveCount(0);
  await expect(page.locator(".file-filter-count")).toContainText("1 file");
});

test("release navigation follows native keyboard order", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const navigation = page.getByRole("navigation", { name: "Project releases" });
  await navigation.getByRole("link", { name: "All releases" }).focus();
  await page.keyboard.press("Tab");
  await expect(navigation.getByRole("link", { name: "1.0.0" })).toBeFocused();
});

test("large histories group without rendering unrelated releases", async ({ page }) => {
  const detail = await page.request.get("/pypi/simple/large-demo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  expect((await detail.json()).files).toHaveLength(2_000);

  await goto(page, "/browse?index=pypi&project=large-demo&version=99.0");
  await expect(page.locator("section.release-files")).toHaveCount(1);
  await expect(page.locator("section.release-files tbody tr")).toHaveCount(20);
  await expect(page.locator(".file-filter-count")).toHaveText("20 files");
});

test("narrow project pages scroll only the file table", async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 800 });
  await goto(page, `${PROJECT_URL}&version=1.0.0`);

  const overflow = await page.evaluate(() => {
    const wrapper = document.querySelector(".release-files .table-scroll");
    return {
      page: document.documentElement.scrollWidth > window.innerWidth + 1,
      table: wrapper.scrollWidth > wrapper.clientWidth,
    };
  });
  expect(overflow).toEqual({ page: false, table: true });
});

test("project file search keeps URL history", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const row = page.locator("table.files tbody tr", { hasText: "veloxdemo-1.0.0" });
  await expect(row).toBeVisible();
  await page.locator(".file-search").fill("missing");
  await expect(page.locator(".release-empty")).toContainText("No artifacts match");
  await expect(page.locator(".file-filter-count")).toContainText("0 of 2 files");
  await expect(page).toHaveURL(/filename=missing/);

  await page.goBack();
  await expect(row).toBeVisible();
  await expect(page.locator(".file-filter-count")).toContainText("2 files");
  await expect(page).toHaveURL(/\/browse\?index=root%2Fpypi&project=veloxdemo$/);

  await page.goForward();
  await expect(page.locator(".release-empty")).toContainText("No artifacts match");
});

test("project file regex errors keep the last results", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const row = page.locator("table.files tbody tr", { hasText: "veloxdemo-1.0.0" });
  await page.locator(".file-search").fill("veloxdemo-1.*\\.whl");
  await page.locator(".file-filter-mode input").check();
  await expect(row).toBeVisible();

  await page.locator(".file-search").fill("[");
  await expect(page.locator(".error")).toContainText("Invalid regex");
  await expect(row).toBeVisible();
  await expect(page.locator(".file-filter-count")).toContainText("1 of 2 files");
});

test("archive browser lists members and shows file content", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await page.getByLabel("Release 1.0.0").getByRole("link", { name: "contents" }).click();
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
  const release = page.locator(".admin-table tr").filter({ hasText: "1.0.0" });

  await release.getByRole("button", { name: "yank", exact: true }).click();
  await expect(page.locator(".outcome")).toContainText("200");
  await expect(page.locator("table.files .badge.yanked-badge")).toBeVisible();

  await release.getByRole("button", { name: "un-yank" }).click();
  await expect(page.locator("table.files .badge.yanked-badge")).toHaveCount(0);
});

test("wrong token surfaces the auth failure", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await page.locator(".admin summary").click();
  await page.locator(".token").fill("wrong");
  await page
    .locator(".admin-table tr")
    .filter({ hasText: "1.0.0" })
    .getByRole("button", { name: "yank", exact: true })
    .click();
  await expect(page.locator(".outcome")).toContainText("401");
});

test("search surfaces provenance facets and the owning index", async ({ page }) => {
  await goto(page, "/search?q=veloxdemo");
  // The results table names the owning index (the renamed vocab, not "repository") and a per-result
  // provenance source badge.
  await expect(page.locator("table.search-results th", { hasText: "Index" })).toBeVisible();
  await expect(page.locator("table.search-results th", { hasText: "Repository" })).toHaveCount(0);
  const row = page.locator("table.search-results tbody tr", { hasText: "veloxdemo" }).first();
  await expect(row).toBeVisible();
  await expect(row.locator("[class*='source-']")).toBeVisible();

  // The uploaded fixture is reachable through the "Uploaded" provenance facet, tagged source-uploaded.
  await goto(page, "/search?q=veloxdemo&type=uploaded");
  const uploaded = page.locator("table.search-results tbody tr", { hasText: "veloxdemo" }).first();
  await expect(uploaded).toBeVisible();
  await expect(uploaded.locator(".badge.source-uploaded")).toBeVisible();

  // The facet select reflects the active facet and offers the renamed provenance vocabulary.
  const select = page.locator(".search-controls select[name='type']");
  await expect(select).toHaveValue("uploaded");
  await expect(select.locator("option")).toContainText(["All", "Uploaded", "Cached", "Override"]);
});

test("usage stats drill from index to project to file", async ({ page }) => {
  // Generate traffic the counters can show: a page view and a file download.
  const detail = await page.request.get("/root/pypi/simple/veloxdemo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  const files = (await detail.json()).files;
  await page.request.get(files[0].url);

  await goto(page, "/");
  const virtualIndex = page.locator(".card", { hasText: "root/pypi" });
  await expect(virtualIndex.locator(".card-usage")).toContainText("downloads");
  await virtualIndex.locator(".card-usage a", { hasText: "usage" }).click();

  await expect(page.locator(".breadcrumb")).toContainText("root/pypi");
  await expect.poll(async () => page.locator(".stats-table tbody tr").count()).toBeGreaterThan(0);
  await page.locator(".stats-table a", { hasText: "veloxdemo" }).click();

  await expect(page.locator(".breadcrumb")).toContainText("veloxdemo");
  await expect(page.locator(".stats-table tbody tr", { hasText: "veloxdemo-1.0.0" }).first()).toBeVisible();
});

test("browses an OCI repository's tags and its manifest", async ({ page }) => {
  await goto(page, "/browse?index=images&project=app");
  // The repository page lists the pushed tag.
  await expect(page.locator(".page")).toContainText("1.0");
  // Clicking the tag opens its manifest, showing the config and layer blob digests.
  await page.getByRole("link", { name: "1.0" }).click();
  await expect(page).toHaveURL(/ref=1\.0/);
  await expect(page.locator(".page")).toContainText("Layers");
  await expect(page.locator(".page")).toContainText("Config: sha256:");
  await expect(page.locator(".page")).toContainText("application/vnd.oci.image.layer.v1.tar");
  // The manifest view offers a copyable pull command, with the host filled in after hydration.
  await expect(page.locator(".install code")).toContainText(`docker pull ${HOST}/images/app:1.0`);
});

test("browses a layer's file contents and previews a text member", async ({ page }) => {
  await goto(page, "/browse?index=images&project=app&ref=1.0");
  // The layer row's contents link opens the archive browser over the layer tar.
  await page.getByRole("link", { name: "contents" }).click();
  await expect(page).toHaveURL(/layer=/);
  await expect(page.locator(".page")).toContainText("etc/app.conf");
  await expect(page.locator(".page")).toContainText("bin/app");
  // A text member previews inline; a binary one does not link.
  await page.getByRole("link", { name: "etc/app.conf" }).click();
  await expect(page.locator(".page")).toContainText("debug = true");
});
