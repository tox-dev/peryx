// Functional tests of the hydrated web UI: every reactive feature is driven the way a person would
// drive it, against a real peryx with a real uploaded package.
import { expect, test } from "@playwright/test";
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const PROJECT_URL = "/browse?index=root%2Fpypi&project=veloxdemo";
const TOKEN = "playwright-secret";
const HOST = `127.0.0.1:${process.env.PERYX_FRONTEND_PORT ?? "4455"}`;
const FIXTURE_WHEEL = join(dirname(fileURLToPath(import.meta.url)), "..", "fixtures", "veloxdemo-1.0.0-py3-none-any.whl");
// The status, dashboard, and stats surfaces filter to the caller's class, so viewing the topology and
// counters needs the bootstrapped administrator's credential on every request the page makes.
const ADMIN_AUTH = `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`;

test.afterEach(async ({ page }, testInfo) => {
  if (!process.env.PERYX_WASM_PROFRAW || page.isClosed() || (await page.locator("body[data-hydrated]").count()) === 0) return;
  const profile = await page.evaluate(async () => {
    const module = await import("/pkg/peryx_web.js");
    return Array.from(module.capture_coverage());
  });
  const identity = `${testInfo.workerIndex}-${testInfo.retry}-${testInfo.titlePath.join("-")}`;
  const digest = createHash("sha256").update(identity).digest("hex");
  writeFileSync(join(process.env.PERYX_WASM_PROFRAW, `${digest}.profraw`), Buffer.from(profile));
});

/// Navigate and wait for the wasm bundle to hydrate, so clicks hit live handlers.
async function goto(page, url) {
  await page.goto(url);
  await page.waitForSelector("body[data-hydrated]");
}

/// A page whose every request carries the administrator credential, so the operator- and
/// administrator-class status, dashboard, and stats fields render.
async function opsPage(browser) {
  const context = await browser.newContext({
    baseURL: `http://${HOST}`,
    extraHTTPHeaders: { authorization: ADMIN_AUTH },
  });
  return context.newPage();
}

async function openUpload(page, route, token, file) {
  await goto(page, "/upload");
  await page.locator("#upload-route").selectOption(route);
  await page.locator("#upload-token").fill(token);
  await page.locator("#upload-file").setInputFiles(file);
}

function policyDecision(state, fresh, project = "blocked-package") {
  return {
    id: `decision-${state}`,
    repository: "internal",
    project,
    version: "1.0",
    filename: `${project}-1.0.whl`,
    source: "pypi",
    action: "serve",
    state,
    rule: "blocked-project",
    reason: "project is blocked",
    evaluated_at_unix: 0,
    input_generation: { repository: 0, catalog: 0, policy: 0 },
    next_eligible_at_unix: null,
    fresh,
  };
}

test("dashboard shows identity, counters, and the topology", async ({ browser }) => {
  const page = await opsPage(browser);
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
  await page.locator(".nav-links a", { hasText: "Upload" }).click();
  await expect(page).toHaveURL(/\/upload$/);
  await page.locator(".nav-links a", { hasText: "Dashboard" }).click();
  await expect(page.locator(".card", { hasText: "root/pypi" })).toBeVisible();
  // External links carry the right targets without being followed.
  await expect(page.locator(".nav-links a", { hasText: "Docs" })).toHaveAttribute("href", /readthedocs/);
  await expect(page.locator(".nav-links a", { hasText: "GitHub" })).toHaveAttribute("href", /github\.com/);
});

test("browser upload publishes through a writable PyPI route", async ({ page }) => {
  await openUpload(page, "zz-browser-upload", TOKEN, FIXTURE_WHEEL);
  await page.locator(".upload-actions button[type='submit']").click();

  await expect(page.locator(".upload-outcome")).toHaveText("veloxdemo-1.0.0-py3-none-any.whl: uploaded");
  const detail = await page.request.get("/zz-browser-upload/simple/veloxdemo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  expect(detail.status()).toBe(200);
  expect(await detail.text()).toContain("veloxdemo-1.0.0-py3-none-any.whl");
});

test("browser upload surfaces authorization denial", async ({ page }) => {
  await openUpload(page, "internal", "wrong-token", {
    name: "denied-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.from("denied"),
  });
  await page.locator(".upload-actions button[type='submit']").click();

  await expect(page.locator(".upload-outcome")).toContainText("denied-1.0-py3-none-any.whl: unauthorized");
  expect((await page.request.get("/internal/simple/denied/")).status()).toBe(404);
});

test("browser upload surfaces archive validation", async ({ page }) => {
  await openUpload(page, "internal", TOKEN, {
    name: "broken-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.from("not a wheel"),
  });
  await page.locator(".upload-actions button[type='submit']").click();

  await expect(page.locator(".upload-outcome")).toContainText("broken-1.0-py3-none-any.whl: uploaded content does not match");
  expect((await page.request.get("/internal/simple/broken/")).status()).toBe(404);
});

test("browser upload applies the configured size limit", async ({ page }) => {
  await openUpload(page, "limited", TOKEN, FIXTURE_WHEEL);
  await page.locator(".upload-actions button[type='submit']").click();

  await expect(page.locator(".upload-outcome")).toContainText("max-file-size");
  expect((await page.request.get("/limited/simple/veloxdemo/")).status()).toBe(404);
});

test("browser upload cancellation publishes no release", async ({ page, context }) => {
  const network = await context.newCDPSession(page);
  await network.send("Network.enable");
  await network.send("Network.emulateNetworkConditions", {
    offline: false,
    latency: 20,
    downloadThroughput: -1,
    uploadThroughput: 1024,
  });
  await openUpload(page, "internal", TOKEN, {
    name: "cancelled-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.alloc(1024 * 1024),
  });
  await page.locator(".upload-actions button[type='submit']").click();
  await expect(page.locator(".upload-outcome")).toContainText("uploading");
  await page.locator(".upload-actions button[type='button']").click();

  await expect(page.locator(".upload-outcome")).toContainText("upload cancelled");
  expect((await page.request.get("/internal/simple/cancelled/")).status()).toBe(404);
});

test("browser upload hides storage internals", async ({ page }) => {
  await page.route("**/internal/", async (route) => {
    if (route.request().method() === "POST") {
      await route.fulfill({ status: 500, body: "temporary path /private/staging" });
    } else {
      await route.continue();
    }
  });
  await openUpload(page, "internal", TOKEN, {
    name: "storage-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.from("storage failure"),
  });
  await page.locator(".upload-actions button[type='submit']").click();

  await expect(page.locator(".upload-outcome")).toHaveText("storage-1.0-py3-none-any.whl: server could not store the upload");
  await expect(page.locator(".upload-outcome")).not.toContainText("/private/staging");
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

test("usage stats page lists indexes and drills into one", async ({ browser }) => {
  const page = await opsPage(browser);
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

test("project page summarizes hosted provenance and flags a mirrored claim", async ({ page }) => {
  await goto(page, PROJECT_URL);
  // The uploaded 1.0.0 wheel carries a hosted, subject-bound attestation.
  const hosted = page
    .locator("table.files tbody tr", { hasText: "veloxdemo-1.0.0-py3-none-any.whl" })
    .locator(".provenance-panel");
  await expect(hosted.locator(".prov-source")).toHaveText("hosted");
  await expect(hosted.locator(".prov-validation")).toHaveText("binding verified");
  // The disclosure is keyboard-operable; opening it reveals each attestation's predicate type and subject.
  await hosted.locator("summary").focus();
  await page.keyboard.press("Enter");
  await expect(hosted.locator(".attestation code.predicate-type")).toContainText("attestations/publish/v1");
  await expect(hosted.locator(".attestation .badge[class*='subject-']")).toHaveText("subject matched");
  await expect(hosted.locator("a.provenance-doc")).toBeVisible();

  // The mirrored 0.9 file advertises a claim peryx neither fetched nor verified; the summary links the
  // upstream document as an external resource without listing any attestation.
  const mirrored = page
    .locator("table.files tbody tr", { hasText: "veloxdemo-0.9-py3-none-any.whl" })
    .locator(".provenance-panel");
  await expect(mirrored.locator(".prov-source")).toHaveText("mirrored");
  await expect(mirrored.locator(".prov-validation")).toHaveText("unverified claim");
  await expect(mirrored.locator("a.provenance-doc")).toHaveAttribute(
    "href",
    /veloxdemo-0\.9-py3-none-any\.whl\.provenance$/,
  );
  await expect(mirrored.locator("a.provenance-doc")).toHaveAttribute("rel", /noopener/);
  await expect(mirrored.locator(".attestation")).toHaveCount(0);
});

test("unknown routes render the not-found fallback", async ({ page }) => {
  // Peryx answers unmatched paths before they reach the SPA shell, so this
  // one skips the hydration wait the other tests rely on.
  const response = await page.goto("/does-not-exist");
  expect(response.status()).toBe(404);
  await expect(page.locator("body")).toContainText("not found");
});

test("admin table shows upstream and upload state per index", async ({ browser }) => {
  const page = await opsPage(browser);
  await goto(page, "/admin/status");
  const table = page.locator(".ops-table").first();
  // The cached index reports a configured upstream; a hosted index shows an upload badge.
  await expect(table.locator(".badge.status-configured").first()).toBeVisible();
  await expect(table.locator("[class*='badge upload-']").first()).toBeVisible();
});

test("admin status is read-only and tolerates failed stats fetches", async ({ browser }) => {
  const page = await opsPage(browser);
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

test("policy decision filters keep credentials out of navigation and render every field", async ({ page }) => {
  await page.route("**/+policy/decisions?**", async (route) => {
    expect(route.request().headers().authorization).toBe(
      `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`,
    );
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    expect(url.searchParams.get("state")).toBe("deny");
    expect(url.search).not.toContain("browser-admin-secret");
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({
        decisions: [
          policyDecision("deny", false),
          {
            ...policyDecision("allow", true, "unversioned-package"),
            version: null,
            filename: null,
            source: null,
            rule: null,
            reason: null,
          },
        ],
        next_cursor: null,
      }),
    });
  });
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator("#policy-repository").fill("internal");
  await page.locator("#policy-state").selectOption("deny");
  await page.locator(".policy-filters button[type='submit']").click();

  const table = page.locator(".policy-decisions-table");
  await expect(table).toContainText("Stale Denied");
  await expect(table).toContainText("blocked-package");
  await expect(table).toContainText("1970-01-01T00:00:00Z");
  await expect(table.getByRole("columnheader")).toHaveText([
    "Outcome",
    "Repository",
    "Package",
    "Version",
    "File",
    "Source",
    "Action",
    "Rule",
    "Reason",
    "Evaluated (UTC)",
    "Next eligible (UTC)",
  ]);
  await expect(table.locator("tbody tr", { hasText: "unversioned-package" }).locator("td")).toHaveText([
    "Allowed",
    "internal",
    "unversioned-package",
    "-",
    "-",
    "-",
    "serve",
    "-",
    "-",
    "1970-01-01T00:00:00Z",
    "-",
  ]);
  await expect(page).toHaveURL(/\/admin\/policy-decisions$/);
  await expect(page.locator("#policy-password")).toHaveAttribute("autocomplete", "off");
  expect(
    await page.evaluate(() =>
      [localStorage, sessionStorage].flatMap((storage) =>
        Array.from({ length: storage.length }, (_, index) => storage.getItem(storage.key(index))),
      ),
    ),
  ).not.toContain("browser-admin-secret");
});

test("policy decision view enforces live administrator and repository-token boundaries", async ({ page }) => {
  await goto(page, "/admin/policy-decisions");

  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.locator(".policy-results")).not.toContainText("Enter credentials and search");
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.locator("#policy-user").fill("__token__");
  await page.locator("#policy-password").fill(TOKEN);
  await page.locator("#policy-repository").fill("internal");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("status")).toContainText(/policy decisions/i);
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.locator("#policy-password").fill("playwright-reader");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("This repository token cannot inspect policy decisions.");

  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("wrong password");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("The username or password was not accepted.");
});

test("policy decision rule text is rendered without markup execution", async ({ page }) => {
  const decision = policyDecision("deny", true);
  decision.rule = '<img src="missing" onerror="window.policyRuleExecuted=true">';
  decision.reason = "<script>window.policyReasonExecuted=true</script>";
  await page.route("**/+policy/decisions?**", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ decisions: [decision], next_cursor: null }),
    }),
  );
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator(".policy-filters button[type='submit']").click();

  const row = page.locator(".policy-decisions-table tbody tr");
  await expect(row.locator("td").nth(7)).toHaveText(decision.rule);
  await expect(row.locator("td").nth(8)).toHaveText(decision.reason);
  await expect(row.locator("img, script")).toHaveCount(0);
  expect(await page.evaluate(() => [window.policyRuleExecuted, window.policyReasonExecuted])).toEqual([
    undefined,
    undefined,
  ]);
});

test("policy decision pagination keeps filters and works from the keyboard", async ({ page }) => {
  let requests = 0;
  await page.route("**/+policy/decisions?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    expect(url.searchParams.get("state")).toBe("deny");
    expect(url.searchParams.get("rule")).toBe("blocked-project");
    const cursor = url.searchParams.get("cursor");
    requests += 1;
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({
        decisions: [policyDecision(cursor === null ? "deny" : "wait", true)],
        next_cursor: cursor === null ? "page-2" : null,
      }),
    });
  });
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-user").focus();
  await page.keyboard.press("Tab");
  await expect(page.locator("#policy-password")).toBeFocused();
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator("#policy-repository").fill("internal");
  await page.locator("#policy-state").selectOption("deny");
  await page.locator("#policy-rule").fill("blocked-project");
  await page.locator(".policy-filters button[type='submit']").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".policy-decisions-table")).toContainText("Denied");

  await page.getByRole("button", { name: "Next" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".policy-decisions-table")).toContainText("Waiting");
  await page.getByRole("button", { name: "Previous" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".policy-decisions-table")).toContainText("Denied");
  expect(requests).toBe(3);
});

test("policy decision view distinguishes an empty result", async ({ page }) => {
  await page.route("**/+policy/decisions?**", async (route) => {
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ decisions: [], next_cursor: null }),
    });
  });
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator("#policy-repository").fill("internal");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("status")).toHaveText("No policy decisions matched these filters.");
});

for (const [name, status, message] of [
  ["invalid filter", 400, "One or more policy decision filters are invalid."],
  ["invalid local credential", 401, "The username or password was not accepted."],
  ["repository token boundary", 403, "This repository token cannot inspect policy decisions."],
  ["repository boundary", 404, "The repository was not found or is not available to this user."],
  ["service failure", 503, "The policy decision service is unavailable."],
]) {
  test(`policy decision view reports ${name} without response text`, async ({ page }) => {
    await page.route("**/+policy/decisions?**", (route) =>
      route.fulfill({ status, body: "secret-package must stay hidden" }),
    );
    await goto(page, "/admin/policy-decisions");
    await page.locator("#policy-user").fill("administrator");
    await page.locator("#policy-password").fill("browser-admin-secret");
    await page.locator(".policy-filters button[type='submit']").click();

    await expect(page.getByRole("alert")).toHaveText(message);
    await expect(page.getByRole("alert")).not.toContainText("secret-package");
  });
}

test("policy decision view reports malformed success data", async ({ page }) => {
  await page.route("**/+policy/decisions?**", (route) => route.fulfill({ status: 200, body: "secret-package" }));
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator(".policy-filters button[type='submit']").click();

  await expect(page.getByRole("alert")).toHaveText("The policy decision service returned invalid data.");
  await expect(page.getByRole("alert")).not.toContainText("secret-package");
});

test("policy decision view reports a network failure", async ({ page }) => {
  await page.route("**/+policy/decisions?**", (route) => route.abort("connectionfailed"));
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  await page.locator(".policy-filters button[type='submit']").click();

  await expect(page.getByRole("alert")).toHaveText("The policy decision service could not be reached.");
});

function shadowCandidate(overrides = {}) {
  return {
    member: "pypi",
    source: "cached",
    filename: "example-1.0-py3-none-any.whl",
    digest: "sha256:abcd",
    selected: false,
    reason: "precedence",
    ...overrides,
  };
}

test("shadow inspection labels outcome, source, and decision without colour alone", async ({ page }) => {
  await page.route("**/+shadow/candidates?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("root/pypi");
    expect(url.searchParams.get("project")).toBe("veloxdemo");
    expect(url.search).not.toContain("browser-admin-secret");
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({
        candidates: [
          shadowCandidate({
            member: "hosted",
            source: "hosted",
            selected: true,
            reason: null,
            decision: { state: "deny", rule: "blocked-project", reason: "project is blocked", evaluated_at_unix: 0, next_eligible_at_unix: null, fresh: true },
          }),
          shadowCandidate({
            decision: { state: "wait", rule: "cooldown", reason: "rate limited", evaluated_at_unix: 0, next_eligible_at_unix: 60, fresh: true },
          }),
          shadowCandidate({ filename: "example-2.0-py3-none-any.whl", selected: true, reason: null }),
        ],
        next_cursor: null,
      }),
    });
  });
  await goto(page, "/admin/shadow");
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("veloxdemo");
  await page.locator(".policy-filters button[type='submit']").click();

  const table = page.locator(".shadow-inspection-table");
  await expect(table.getByRole("columnheader")).toHaveText([
    "Outcome",
    "Decision",
    "Source",
    "Member",
    "File",
    "Digest",
    "Shadowed because",
    "Rule",
    "Reason",
    "Next eligible (UTC)",
  ]);
  await expect(table).toContainText("Selected");
  await expect(table).toContainText("Shadowed");
  await expect(table).toContainText("Denied");
  await expect(table).toContainText("Waiting");
  await expect(table).toContainText("hosted upload");
  await expect(table).toContainText("cached upstream");
  await expect(table).toContainText("Higher-precedence member");
  await expect(table).toContainText("1970-01-01T00:01:00Z");
  const undecided = table.locator("tbody tr", { hasText: "example-2.0-py3-none-any.whl" });
  await expect(undecided.locator("td").nth(1)).toHaveText("-");
});

test("shadow inspection escapes policy text and leaks no upstream url", async ({ page }) => {
  const candidate = shadowCandidate({
    reason: "fallback",
    decision: {
      state: "deny",
      rule: '<img src="missing" onerror="window.shadowRuleExecuted=true">',
      reason: "<script>window.shadowReasonExecuted=true</script>",
      evaluated_at_unix: 0,
      next_eligible_at_unix: null,
      fresh: false,
    },
  });
  await page.route("**/+shadow/candidates?**", (route) =>
    route.fulfill({ contentType: "application/json", body: JSON.stringify({ candidates: [candidate], next_cursor: null }) }),
  );
  await goto(page, "/admin/shadow");
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("veloxdemo");
  await page.locator(".policy-filters button[type='submit']").click();

  const row = page.locator(".shadow-inspection-table tbody tr");
  await expect(row).toContainText("Stale Denied");
  await expect(row.locator("td").nth(7)).toHaveText(candidate.decision.rule);
  await expect(row.locator("td").nth(8)).toHaveText(candidate.decision.reason);
  await expect(row.locator("img, script")).toHaveCount(0);
  expect(await page.evaluate(() => [window.shadowRuleExecuted, window.shadowReasonExecuted])).toEqual([undefined, undefined]);
  await expect(page.locator(".shadow-inspection-table")).not.toContainText("http");
});

test("shadow inspection distinguishes an empty result", async ({ page }) => {
  await page.route("**/+shadow/candidates?**", (route) =>
    route.fulfill({ contentType: "application/json", body: JSON.stringify({ candidates: [], next_cursor: null }) }),
  );
  await goto(page, "/admin/shadow");
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("missing");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("status")).toHaveText("No candidates resolved for this repository and project.");
});

test("shadow inspection pages a large project from the keyboard on a narrow screen", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 820 });
  await page.route("**/+shadow/candidates?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("root/pypi");
    expect(url.searchParams.get("project")).toBe("large-demo");
    const cursor = url.searchParams.get("cursor");
    const candidates = Array.from({ length: 100 }, (_, index) =>
      shadowCandidate({ filename: `large-${String(index).padStart(3, "0")}.whl`, selected: index === 0, reason: index === 0 ? null : "precedence" }),
    );
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ candidates, next_cursor: cursor === null ? "page-2" : null }),
    });
  });
  await goto(page, "/admin/shadow");
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-user").focus();
  await page.keyboard.press("Tab");
  await expect(page.locator("#shadow-password")).toBeFocused();
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("large-demo");
  await page.locator(".policy-filters button[type='submit']").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".shadow-inspection-table tbody tr")).toHaveCount(100);
  await expect(page.locator(".shadow-inspection-page .table-scroll")).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1)).toBe(false);

  await page.getByRole("button", { name: "Next" }).focus();
  await page.keyboard.press("Enter");
  // Both buttons disable while a page loads, so "Next disabled" alone cannot tell a settled last page
  // from an in-flight fetch. Page two is the last page (no next cursor) and has a previous page, so
  // its settled state is Next disabled with Previous enabled. Waiting for Previous to enable proves
  // the fetch finished before the keyboard Previous below, which a disabled button would otherwise drop.
  await expect(page.getByRole("button", { name: "Next" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Previous" })).toBeEnabled();
  await page.getByRole("button", { name: "Previous" }).focus();
  await page.keyboard.press("Enter");
  // Back on page one: the rows re-render, Previous disables (no earlier page), and Next re-enables
  // from the restored next cursor. Asserting the settled navigation state instead of a mid-race fetch
  // tally survives an extra deduped request that does not change what the user sees.
  await expect(page.locator(".shadow-inspection-table tbody tr")).toHaveCount(100);
  await expect(page.getByRole("button", { name: "Previous" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Next" })).toBeEnabled();
});

test("shadow inspection enforces live administrator and repository-token boundaries", async ({ page }) => {
  await goto(page, "/admin/shadow");

  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("veloxdemo");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.locator(".policy-results")).not.toContainText("Enter credentials");
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.locator("#shadow-user").fill("__token__");
  await page.locator("#shadow-password").fill("playwright-reader");
  await page.locator("#shadow-repository").fill("internal");
  await page.locator("#shadow-project").fill("veloxdemo");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("This repository token cannot inspect shadowed candidates.");

  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("wrong password");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("The username or password was not accepted.");
});

test("shadow inspection rejects a blank repository or project before any request", async ({ page }) => {
  let requested = false;
  await page.route("**/+shadow/candidates?**", (route) => {
    requested = true;
    return route.fulfill({ contentType: "application/json", body: JSON.stringify({ candidates: [], next_cursor: null }) });
  });
  await goto(page, "/admin/shadow");
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.evaluate(() => {
    for (const field of ["#shadow-repository", "#shadow-project"]) document.querySelector(field).removeAttribute("required");
  });
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("Enter a repository and a project to inspect.");
  expect(requested).toBe(false);
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

test("admin topology table fits the page and uses current vocabulary", async ({ browser }) => {
  const page = await opsPage(browser);
  await goto(page, "/admin/status");
  // Renamed heading and the merged role x ecosystem "Type" column.
  await expect(page.locator(".ops-page h2", { hasText: "Indexes" })).toBeVisible();
  await expect(page.locator(".ops-page h2", { hasText: "Repositories" })).toHaveCount(0);
  await expect(page.locator(".ops-table th", { hasText: "Type" }).first()).toBeVisible();
  await expect(page.locator(".ops-table .ops-type").first().locator(".badge")).toHaveCount(2);
  // Wide data tables scroll within their container to keep the page body fixed.
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
  await page.goto("/browse?index=pypi&project=large-demo&version=99.0");
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

test("usage stats drill from index to project to file", async ({ browser }) => {
  const page = await opsPage(browser);
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

function usageEnvelope(rowsKey, rows, { nextCursor = null, clamped = false, retainedFromDay = null } = {}) {
  return {
    [rowsKey]: rows,
    interval: {
      from_day: 19000,
      to_day: 19030,
      from_unix: 19000 * 86400,
      to_unix: 19031 * 86400,
      retained_from_day: retainedFromDay,
      window_clamped_to_retention: clamped,
    },
    next_cursor: nextCursor,
  };
}

async function searchAnalytics(page, { user = "administrator", password = "browser-admin-secret" } = {}) {
  await page.locator("#analytics-user").fill(user);
  await page.locator("#analytics-password").fill(password);
  await page.locator(".analytics-filters button[type='submit']").click();
}

test("usage analytics maps filters to the API, keeps credentials out of the URL, and renders ties", async ({ page }) => {
  await page.route("**/+analytics/top-packages?**", async (route) => {
    expect(route.request().headers().authorization).toBe(
      `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`,
    );
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    expect(url.searchParams.get("from")).toBe(String(Date.UTC(2024, 0, 2) / 1000));
    expect(url.searchParams.get("to")).toBe(String(Date.UTC(2024, 0, 9) / 1000));
    expect(url.searchParams.get("limit")).toBe("25");
    expect(url.search).not.toContain("browser-admin-secret");
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(
        usageEnvelope("packages", [
          { repository: "internal", project: "alpha", downloads: 40, bytes: 2048 },
          { repository: "internal", project: "beta", downloads: 40, bytes: 1024 },
        ]),
      ),
    });
  });
  await goto(page, "/admin/analytics");
  await page.locator("#analytics-repository").fill("internal");
  await page.locator("#analytics-from").fill("2024-01-02");
  await page.locator("#analytics-to").fill("2024-01-09");
  await searchAnalytics(page);

  const table = page.locator(".usage-top-table");
  await expect(table.getByRole("columnheader")).toHaveText(["Repository", "Package", "Downloads", "Bytes"]);
  // Ties keep the server's order rather than being reshuffled by the client.
  await expect(table.locator("tbody tr").nth(0)).toContainText("alpha");
  await expect(table.locator("tbody tr").nth(1)).toContainText("beta");
  await expect(page.locator(".usage-interval")).toContainText("UTC, inclusive");
  await expect(page.getByRole("status")).toContainText("Loaded 2 rows.");
  await expect(page).toHaveURL(/\/admin\/analytics$/);
  await expect(page.locator("#analytics-password")).toHaveAttribute("autocomplete", "off");
  expect(
    await page.evaluate(() =>
      [localStorage, sessionStorage].flatMap((storage) =>
        Array.from({ length: storage.length }, (_, index) => storage.getItem(storage.key(index))),
      ),
    ),
  ).not.toContain("browser-admin-secret");
});

for (const [view, endpoint, rowsKey, row, headers] of [
  [
    "versions",
    "versions",
    "versions",
    { repository: "internal", project: "alpha", version: null, downloads: 3, bytes: 90 },
    ["Repository", "Package", "Version", "Downloads", "Bytes"],
  ],
  [
    "sources",
    "sources",
    "sources",
    { repository: "internal", project: "alpha", source: null, downloads: 3, bytes: 90 },
    ["Repository", "Package", "Source", "Downloads", "Bytes"],
  ],
  [
    "unused",
    "unused",
    "unused",
    { repository: "internal", project: "gamma", lifetime_downloads: 12 },
    ["Repository", "Package", "Lifetime downloads"],
  ],
  [
    "timeline",
    "timeline",
    "buckets",
    { day: 19000, start_unix: 19000 * 86400, end_unix: 19001 * 86400, downloads: 5, bytes: 500 },
    ["Start (UTC)", "End (UTC)", "Downloads", "Bytes"],
  ],
]) {
  test(`usage analytics renders the ${view} view with its own columns`, async ({ page }) => {
    await page.route(`**/+analytics/${endpoint}?**`, (route) =>
      route.fulfill({ contentType: "application/json", body: JSON.stringify(usageEnvelope(rowsKey, [row])) }),
    );
    await goto(page, "/admin/analytics");
    await page.locator("#analytics-view").selectOption(view);
    await searchAnalytics(page);
    const table = page.locator(".usage-table");
    await expect(table.getByRole("columnheader")).toHaveText(headers);
    await expect(table.locator("caption")).toHaveText(/1 /);
    // The placeholder keeps absent values visible to assistive technology.
    if (view === "versions") await expect(table.locator("tbody td").nth(2)).toHaveText("-");
    if (view === "sources") await expect(table.locator("tbody td").nth(2)).toHaveText("local store");
    if (view === "timeline") await expect(table.locator("tbody td").nth(0)).toContainText("T00:00:00Z");
  });
}

test("usage analytics distinguishes an empty window from one clamped to retention", async ({ page }) => {
  let clamped = false;
  await page.route("**/+analytics/top-packages?**", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(usageEnvelope("packages", [], { clamped, retainedFromDay: clamped ? 19010 : null })),
    }),
  );
  await goto(page, "/admin/analytics");
  await searchAnalytics(page);
  await expect(page.getByRole("status")).toContainText("No usage recorded");
  await expect(page.getByRole("note")).toHaveCount(0);

  clamped = true;
  await searchAnalytics(page);
  await expect(page.getByRole("note")).toContainText("Window clamped to retention");
  await expect(page.getByRole("note")).toContainText("aged out");
  await expect(page.getByRole("status")).toContainText("No usage recorded");
});

test("usage analytics pagination keeps the view and filters and works from the keyboard", async ({ page }) => {
  let requests = 0;
  await page.route("**/+analytics/versions?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    const cursor = url.searchParams.get("cursor");
    requests += 1;
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(
        usageEnvelope(
          "versions",
          [{ repository: "internal", project: cursor === null ? "first" : "second", version: "1.0", downloads: 1, bytes: 2 }],
          { nextCursor: cursor === null ? "page-2" : null },
        ),
      ),
    });
  });
  await goto(page, "/admin/analytics");
  await page.locator("#analytics-view").selectOption("versions");
  await page.locator("#analytics-repository").fill("internal");
  await searchAnalytics(page);
  await expect(page.locator(".usage-table")).toContainText("first");

  await page.getByRole("button", { name: "Next" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".usage-table")).toContainText("second");
  await page.getByRole("button", { name: "Previous" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".usage-table")).toContainText("first");
  expect(requests).toBe(3);
});

for (const [name, status, message] of [
  ["invalid filter", 400, "One or more analytics filters are invalid."],
  ["invalid local credential", 401, "The username or password was not accepted."],
  ["repository token boundary", 403, "This repository token cannot inspect usage analytics."],
  ["repository boundary", 404, "The repository was not found or is not available to this user."],
  ["service failure", 503, "The analytics service is unavailable."],
]) {
  test(`usage analytics reports ${name} without response text`, async ({ page }) => {
    await page.route("**/+analytics/top-packages?**", (route) =>
      route.fulfill({ status, body: "secret-package must stay hidden" }),
    );
    await goto(page, "/admin/analytics");
    await searchAnalytics(page);
    await expect(page.getByRole("alert")).toHaveText(message);
    await expect(page.getByRole("alert")).not.toContainText("secret-package");
  });
}

test("usage analytics reports malformed success data and a network failure", async ({ page }) => {
  await page.route("**/+analytics/top-packages?**", (route) => route.fulfill({ status: 200, body: "secret-package" }));
  await goto(page, "/admin/analytics");
  await searchAnalytics(page);
  await expect(page.getByRole("alert")).toHaveText("The analytics service returned invalid data.");
  await expect(page.getByRole("alert")).not.toContainText("secret-package");

  await page.unroute("**/+analytics/top-packages?**");
  await page.route("**/+analytics/top-packages?**", (route) => route.abort("connectionfailed"));
  await searchAnalytics(page);
  await expect(page.getByRole("alert")).toHaveText("The analytics service could not be reached.");
});

test("usage analytics enforces live operator and repository-token boundaries", async ({ page }) => {
  await goto(page, "/admin/analytics");
  // A live administrator holds operator analytics scope, so the default top view resolves.
  await searchAnalytics(page);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(page.getByRole("status")).toBeVisible();

  // The source split is operator-only: a repository upload token cannot reach it.
  await page.locator("#analytics-user").fill("__token__");
  await page.locator("#analytics-password").fill(TOKEN);
  await page.locator("#analytics-repository").fill("internal");
  await page.locator("#analytics-view").selectOption("sources");
  await page.locator(".analytics-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("This repository token cannot inspect usage analytics.");

  await page.locator("#analytics-user").fill("administrator");
  await page.locator("#analytics-password").fill("wrong password");
  await page.locator("#analytics-view").selectOption("top");
  await page.locator("#analytics-repository").fill("");
  await page.locator(".analytics-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText("The username or password was not accepted.");
});

test("usage analytics nav link reaches the route", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Usage" }).click();
  await expect(page).toHaveURL(/\/admin\/analytics$/);
  await expect(page.locator("h1", { hasText: "Usage analytics" })).toBeVisible();
});

// The administrator view of the availability topology: a local writer plus two replicas in distinct
// health states. A client-side nav to the page runs the loader in the browser, so this mock stands in
// for the `/+availability/topology` snapshot without configuring a live roster in the fixture.
const topologySnapshot = {
  mode: "dc",
  group: "east",
  captured_at: 1_800_000_000,
  node_count: 3,
  local: { role: "writer", liveness: "live", frontier: 42 },
  nodes: [
    { node: "writer-a", dc: "east-1", role: "writer", local: true, liveness: "live", frontier: 42, address: "writer-a.internal:8443" },
    { node: "replica-b", dc: "east-2", role: "replica", local: false, liveness: "unknown", address: "replica-b.internal:8443" },
    { node: "replica-c", dc: "east-3", role: "replica", local: false, liveness: "unready", address: "replica-c.internal:8443" },
  ],
};

test("availability topology renders roster health and filters by role", async ({ page }) => {
  await page.route("**/+availability/topology", (route) =>
    route.fulfill({ contentType: "application/json", body: JSON.stringify(topologySnapshot) }),
  );
  // The page opens a live stream after hydration; feed it the same snapshot so the live update is a
  // no-op over the mocked roster instead of the fixture's empty public one.
  await page.route("**/+availability/topology/stream", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: `id: 1\nevent: topology\ndata: ${JSON.stringify(topologySnapshot)}\n\n`,
    }),
  );
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Topology" }).click();
  await expect(page).toHaveURL(/\/admin\/topology$/);
  await expect(page.locator("h1", { hasText: "Availability topology" })).toBeVisible();
  // The live feed badge appears once the browser subscribes to the stream after hydration.
  await expect(page.locator(".ops-title .badge", { hasText: "feed:" })).toBeVisible();

  const rows = page.locator(".topology-table tbody tr");
  await expect(rows).toHaveCount(3);
  await expect(page.locator(".topology-table")).toContainText("writer-a");
  await expect(page.locator(".topology-table")).toContainText("replica-b");
  // Every health state carries its own word, so the roster reads correctly without colour.
  await expect(page.locator(".topology-table .health-live")).toHaveText("Live");
  await expect(page.locator(".topology-table .health-unknown")).toHaveText("Unknown");
  await expect(page.locator(".topology-table .health-unready")).toHaveText("Unready");
  await expect(page.locator(".topology-table")).toContainText("replica-b.internal:8443");

  await page.locator("#topology-role").selectOption("replica");
  await expect(rows).toHaveCount(2);
  await expect(page.locator(".topology-table")).not.toContainText("writer-a");
  await expect(page.locator(".result-count")).toHaveText("Showing 2 of 3 roster nodes.");

  await page.locator("#topology-role").selectOption("writer");
  await expect(rows).toHaveCount(1);
  await expect(page.locator(".topology-table")).toContainText("writer-a");
  await expect(page.locator(".topology-table")).not.toContainText("replica-b");

  await page.locator("#topology-role").selectOption("all");
  await expect(rows).toHaveCount(3);
});

async function mockTopologyStream(page) {
  await page.addInitScript(() => {
    class MockEventSource {
      static CONNECTING = 0;
      static OPEN = 1;
      static CLOSED = 2;

      constructor() {
        this.readyState = MockEventSource.CONNECTING;
        this.listeners = new Map();
        window.__topologyStream = this;
      }

      addEventListener(type, listener) {
        this.listeners.set(type, listener);
      }

      close() {
        this.readyState = MockEventSource.CLOSED;
      }

      open() {
        this.readyState = MockEventSource.OPEN;
        this.onopen?.(new Event("open"));
      }

      message(data) {
        this.listeners.get("topology")?.(new MessageEvent("topology", { data }));
      }

      drop() {
        this.readyState = MockEventSource.CONNECTING;
        this.onerror?.(new Event("error"));
      }
    }
    window.EventSource = MockEventSource;
  });
}

async function openTopology(page) {
  await page.route("**/+availability/topology", (route) =>
    route.fulfill({ contentType: "application/json", body: JSON.stringify(topologySnapshot) }),
  );
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Topology" }).click();
  await expect(page).toHaveURL(/\/admin\/topology$/);
  return page.locator(".ops-title .badge", { hasText: "feed:" });
}

test("topology feed stays out of live until the stream opens", async ({ page }) => {
  await mockTopologyStream(page);
  const badge = await openTopology(page);
  await expect(badge).toHaveText("feed: Reconnecting");
  await page.evaluate(() => window.__topologyStream.open());
  await expect(badge).toHaveText("feed: Live");
});

test("topology feed leaves live when the stream sends undecodable data", async ({ page }) => {
  await mockTopologyStream(page);
  const badge = await openTopology(page);
  await page.evaluate(() => {
    window.__topologyStream.open();
    window.__topologyStream.message("not a snapshot");
  });
  await expect(badge).toHaveText("feed: Stale");
});

test("topology feed reports reconnecting after a drop and recovers to live", async ({ page }) => {
  await mockTopologyStream(page);
  const badge = await openTopology(page);
  await page.evaluate(() => {
    window.__topologyStream.open();
    window.__topologyStream.drop();
  });
  await expect(badge).toHaveText("feed: Reconnecting");
  await page.evaluate(() => window.__topologyStream.open());
  await expect(badge).toHaveText("feed: Live");
});
