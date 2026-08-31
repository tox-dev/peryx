import { expect, test } from "@playwright/test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  ADMIN_AUTH,
  browserErrors,
  collectWasmCoverage,
  goto,
  openUpload,
  operatorPage,
  verifyBrowser,
} from "../test-support.mjs";

const PROJECT_URL = "/browse?index=root%2Fpypi&project=veloxdemo";
const TOKEN = "playwright-secret";
const FIXTURE_WHEEL = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "fixtures",
  "veloxdemo-1.0.0-py3-none-any.whl",
);

collectWasmCoverage(test);
test.beforeAll(async ({ browser }) => verifyBrowser(browser));

function browseSection(page, heading) {
  return page.locator(".browse-section").filter({
    has: page.getByRole("heading", { name: heading, exact: true }),
  });
}

async function navigateClient(page, url) {
  await page.evaluate((next) => {
    history.pushState({}, "", next);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, url);
}

function isPolicyResponse(response) {
  return new URL(response.url()).pathname.endsWith("/+policy/decisions");
}

async function submitPolicy(page) {
  const pending = page.waitForResponse(isPolicyResponse);
  await page.locator(".policy-filters button[type='submit']").click();
  return pending;
}

async function gotoShadow(page) {
  await page.goto("/admin/shadow");
  await expect(
    page.getByRole("heading", { name: "PyPI shadow inspection" }),
  ).toBeVisible();
}

async function expectPolicyResult(page, response) {
  const { decisions } = await response.json();
  await expect(page.getByRole("status")).toHaveText(
    decisions.length
      ? `Loaded ${decisions.length} policy decisions.`
      : "No policy decisions matched these filters.",
  );
}

async function searchAnalytics(
  page,
  { user = "administrator", password = "browser-admin-secret" } = {},
) {
  await page.locator("#analytics-user").fill(user);
  await page.locator("#analytics-password").fill(password);
  await page.locator(".analytics-filters button[type='submit']").click();
}

test("dashboard shows identity, counters, and the topology", async ({
  browser,
}) => {
  const page = await operatorPage(browser);
  await goto(page, "/");
  const globalGroup = page.locator(".metrics-group", { hasText: "Global" });
  await expect(
    globalGroup.locator(".stat", { hasText: "accepted requests" }),
  ).toBeVisible();
  const pypiGroup = page.locator(".metrics-group", {
    has: page.locator(".badge.ecosystem-pypi"),
  });
  await expect(
    pypiGroup.locator(".stat", { hasText: "listings served" }),
  ).toBeVisible();
  await expect(
    pypiGroup.locator(".stat", { hasText: "PEP 658 metadata hits" }),
  ).toBeVisible();
  await expect(globalGroup).not.toContainText("PEP 658");
  const virtualIndex = page.locator(".card", { hasText: "root/pypi" });
  await expect(virtualIndex.locator(".badge.kind-virtual")).toBeVisible();
  await expect(virtualIndex.locator(".layer")).toHaveCount(2);
  const hostedLayer = virtualIndex.locator(".layer").first();
  await expect(hostedLayer).toContainText("hosted");
  await expect(hostedLayer.locator(".badge.kind-hosted")).toBeVisible();
  await expect(hostedLayer).toContainText("writes land here");
  await expect(
    virtualIndex.locator(".layer").nth(1).locator(".badge.kind-cached"),
  ).toBeVisible();
  await expect(virtualIndex.locator(".layer-hint")).toContainText(
    "first file match wins",
  );
  await expect(
    page.locator("h2", { hasText: "Standalone indexes" }),
  ).toBeVisible();
  const standalone = page.locator(".card", { hasText: "internal" });
  await expect(standalone.locator(".badge.kind-hosted")).toBeVisible();
  await expect(standalone.locator(".badge.uploads")).toBeVisible();
});

test("header nav links reach each in-app route", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Search" }).click();
  await expect(page).toHaveURL(/\/search/);
  await page.locator(".nav-links a", { hasText: "Status" }).click();
  await expect(page).toHaveURL(/\/admin\/status$/);
  await page.locator(".nav-links a", { hasText: "Dashboard" }).click();
  await expect(page.locator(".card", { hasText: "root/pypi" })).toBeVisible();
  await expect(
    page.locator(".nav-links a", { hasText: "Docs" }),
  ).toHaveAttribute("href", /readthedocs/);
  await expect(
    page.locator(".nav-links a", { hasText: "GitHub" }),
  ).toHaveAttribute("href", /github\.com/);
});

test("browser upload publishes through a writable PyPI route", async ({
  page,
}) => {
  await openUpload(page, "zz-browser-upload", TOKEN, FIXTURE_WHEEL);
  await page.locator("#submit").click();

  await expect(page.locator("#outcome")).toHaveText(
    "veloxdemo-1.0.0-py3-none-any.whl: uploaded",
  );
  const detail = await page.request.get(
    "/zz-browser-upload/simple/veloxdemo/",
    {
      headers: { accept: "application/vnd.pypi.simple.v1+json" },
    },
  );
  expect(detail.status()).toBe(200);
  expect(await detail.text()).toContain("veloxdemo-1.0.0-py3-none-any.whl");
});

test("browser upload surfaces authorization denial", async ({ page }) => {
  await openUpload(page, "internal", "wrong-token", {
    name: "denied-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.from("denied"),
  });
  await page.locator("#submit").click();

  await expect(page.locator("#outcome")).toHaveText("unauthorized");
  expect((await page.request.get("/internal/simple/denied/")).status()).toBe(
    404,
  );
});

test("browser upload surfaces archive validation", async ({ page }) => {
  await openUpload(page, "internal", TOKEN, {
    name: "broken-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.from("not a wheel"),
  });
  await page.locator("#submit").click();

  await expect(page.locator("#outcome")).toContainText(
    "uploaded content does not match",
  );
  expect((await page.request.get("/internal/simple/broken/")).status()).toBe(
    404,
  );
});

test("browser upload applies the configured size limit", async ({ page }) => {
  await openUpload(page, "limited", TOKEN, FIXTURE_WHEEL);
  await page.locator("#submit").click();

  await expect(page.locator("#outcome")).toContainText("max-file-size");
  expect((await page.request.get("/limited/simple/veloxdemo/")).status()).toBe(
    404,
  );
});

test("browser upload cancellation publishes no release", async ({
  page,
  context,
}) => {
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
  await page.locator("#submit").click();
  await expect(page.locator("#outcome")).toContainText("uploading");
  await page.locator("#cancel").click();

  await expect(page.locator("#outcome")).toHaveText("Upload cancelled.");
  expect((await page.request.get("/internal/simple/cancelled/")).status()).toBe(
    404,
  );
});

test("browser upload hides storage internals", async ({ page }) => {
  await page.route("**/internal/", async (route) => {
    if (route.request().method() === "POST") {
      await route.fulfill({
        status: 500,
        body: "temporary path /private/staging",
      });
    } else {
      await route.continue();
    }
  });
  await openUpload(page, "internal", TOKEN, {
    name: "storage-1.0-py3-none-any.whl",
    mimeType: "application/octet-stream",
    buffer: Buffer.from("storage failure"),
  });
  await page.locator("#submit").click();

  await expect(page.locator("#outcome")).toHaveText(
    "storage-1.0-py3-none-any.whl: server could not store the upload",
  );
  await expect(page.locator("#outcome")).not.toContainText(
    "/private/staging",
  );
});

test("header search lists packages and opens a result", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".header-search input[name='q']").fill("velox");
  const suggestions = page.locator(".suggestions");
  await expect(suggestions).toBeVisible();
  const item = suggestions
    .locator("a.suggestion", { hasText: "veloxdemo" })
    .first();
  await expect(item).toBeVisible();
  await expect(item.locator("[class*='source-']")).toBeVisible();
  await expect(suggestions.locator("a.all-results")).toBeVisible();
  await item.click();
  await expect(page).toHaveURL(/\/browse\?index=hosted$/);
  await expect(page.locator(".browse-head h1")).toContainText("hosted");
});

test("search reports no matches and honors the provenance facet", async ({
  page,
}) => {
  await goto(page, "/");
  await page
    .locator(".header-search input[name='q']")
    .fill("zzznotapackage");
  await page.locator(".suggestions a.all-results").click();
  await expect(page.locator(".search-page")).toContainText(
    "Nothing matched this search",
  );
  await navigateClient(page, "/search?q=large-demo&type=uploaded");
  await expect(page.locator(".search-page")).toContainText(
    "Nothing matched this search",
  );
  await expect(page).toHaveURL(/type=uploaded/);
});

test("search form submission navigates with the query", async ({ page }) => {
  const errors = browserErrors(page);
  await goto(page, "/search");
  await page.locator(".search-controls input[name='q']").fill("veloxdemo");
  await page.locator(".search-controls button[type='submit']").click();
  await expect(page).toHaveURL(/q=veloxdemo/);
  await expect(
    page
      .locator("table.search-results tbody tr", { hasText: "veloxdemo" })
      .first(),
  ).toBeVisible();
  expect(errors).toEqual([]);
});

test("usage stats page lists indexes and drills into one", async ({
  browser,
}) => {
  const page = await operatorPage(browser);
  await page.request.get("/root/pypi/simple/veloxdemo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  await goto(page, "/stats");
  await expect(page.locator(".breadcrumb")).toContainText("usage");
  await expect(page.locator(".stats-table tbody tr").first()).toBeVisible();
  await page
    .locator(".stats-table a", { hasText: "root/pypi" })
    .first()
    .click();
  await expect(page).toHaveURL(/\/stats\?index=/);
  await expect(page.locator(".breadcrumb")).toContainText("root/pypi");
});

test("project server render includes the request origin", async ({
  page,
}) => {
  const response = await page.request.get(PROJECT_URL);
  const origin = new URL(response.url()).origin;
  expect(await response.text()).toContain(
    `uv pip install --index-url ${origin}/root/pypi/simple/ veloxdemo==1.0.0`,
  );
});

test("project install snippet copies the rendered command", async ({
  page,
}) => {
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await goto(page, PROJECT_URL);
  const origin = new URL(page.url()).origin;
  const command = `uv pip install --index-url ${origin}/root/pypi/simple/ veloxdemo==1.0.0`;
  await expect(page.locator(".install code")).toHaveText(command);
  const copied = page.evaluate(
    () =>
      new Promise((resolve) => {
        const clipboard = navigator.clipboard;
        const writeText = clipboard.writeText.bind(clipboard);
        clipboard.writeText = async (text) => {
          await writeText(text);
          resolve(text);
        };
      }),
  );
  await page.locator(".install button.copy").click();
  expect(await copied).toBe(command);
});

test("project page opens browsable artifacts", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const href = await page
    .locator(".browse-table tbody tr td a", { hasText: /\.whl$/ })
    .first()
    .getAttribute("href");
  const archive = await page.request.get(href);
  expect(archive.status()).toBe(200);
});

test("project page summarizes hosted provenance and flags a mirrored claim", async ({
  page,
}) => {
  await goto(page, PROJECT_URL);
  const hosted = page
    .locator(".browse-table tbody tr", {
      hasText: "veloxdemo-1.0.0-py3-none-any.whl",
    })
    .locator(".badge.provenance-valid");
  await expect(hosted).toHaveText("hosted provenance");
  await expect(hosted).toHaveAttribute(
    "title",
    /attestations\/publish\/v1.*matched/,
  );

  const mirrored = page
    .locator(".browse-table tbody tr", {
      hasText: "veloxdemo-0.9-py3-none-any.whl",
    })
    .locator(".badge", { hasText: "upstream provenance" });
  await expect(mirrored).toHaveText("upstream provenance");
});

test("unknown routes render the not-found fallback", async ({ page }) => {
  const response = await page.goto("/does-not-exist");
  expect(response.status()).toBe(404);
  await expect(page.locator("body")).toContainText("not found");
});

test("admin table shows upstream and upload state per index", async ({
  page,
}) => {
  await operatorPage(page);
  await goto(page, "/admin/status");
  const table = page.locator(".ops-table").first();
  await expect(table.locator(".badge.status-configured").first()).toBeVisible();
  await expect(table.locator("[class*='badge upload-']").first()).toBeVisible();
});

test("admin status is read-only and reports failed stats fetches", async ({
  page,
}) => {
  await operatorPage(page);
  const snapshot = await page.request.get("/+status", {
    headers: { authorization: ADMIN_AUTH },
  });
  expect(snapshot.status()).toBe(200);
  const snapshotBody = await snapshot.body();
  await page.route("**/+status", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: snapshotBody,
    }),
  );
  await page.route("**/+stats**", (route) =>
    route.fulfill({ status: 503, body: "{}" }),
  );
  await goto(page, "/");
  const status = page.waitForResponse(
    (response) => new URL(response.url()).pathname === "/+status",
  );
  const stats = page.waitForResponse(
    (response) => new URL(response.url()).pathname === "/+stats",
  );
  await page.locator(".nav-links a", { hasText: "Status" }).click();
  const [statusResponse, statsResponse] = await Promise.all([status, stats]);
  expect(statusResponse.status()).toBe(200);
  expect(statsResponse.status()).toBe(503);

  await expect(page).toHaveURL(/\/admin\/status$/);
  await expect(page.locator(".ops-title")).toContainText("read-only");
  const topology = page.locator(".ops-table").first();
  await expect(topology).toContainText("root/pypi");
  await expect(topology.locator(".badge.ecosystem-pypi").first()).toBeVisible();
  await expect(topology.locator(".badge.kind-cached")).toBeVisible();
  await expect(topology.locator(".badge.kind-hosted").first()).toBeVisible();
  await expect(topology.locator(".badge.kind-virtual")).toBeVisible();
  await expect(
    page.locator(".ops-table", { hasText: "veloxdemo-1.0.0" }),
  ).toBeVisible();
  await expect(page.locator(".ops-table").first()).not.toContainText(TOKEN);
  await expect(page.getByRole("alert")).toHaveText(
    "/+stats returned HTTP 503.",
  );
  await expect(page.getByText("No usage recorded yet.")).toHaveCount(0);
  await expect(page.locator(".token")).toHaveCount(0);
  await expect(page.locator(".admin-table")).toHaveCount(0);
});

test("policy decisions enforce administrator and repository-token boundaries", async ({
  page,
}) => {
  await goto(page, "/admin/policy-decisions");

  await page.locator("#policy-user").fill("administrator");
  await page.locator("#policy-password").fill("browser-admin-secret");
  const administrator = await submitPolicy(page);
  expect(administrator.status()).toBe(200);
  await expectPolicyResult(page, administrator);
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.locator("#policy-password").fill("wrong password");
  expect((await submitPolicy(page)).status()).toBe(401);
  await expect(page.getByRole("alert")).toHaveText(
    "The username or password was not accepted.",
  );

  await page.locator("#policy-user").fill("__token__");
  await page.locator("#policy-password").fill(TOKEN);
  await page.locator("#policy-repository").fill("internal");
  const repositoryToken = await submitPolicy(page);
  expect(repositoryToken.status()).toBe(200);
  await expectPolicyResult(page, repositoryToken);
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.locator("#policy-password").fill("playwright-reader");
  expect((await submitPolicy(page)).status()).toBe(403);
  await expect(page.getByRole("alert")).toHaveText(
    "This repository token cannot inspect policy decisions.",
  );
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

test("shadow inspection labels outcome, source, and decision without colour alone", async ({
  page,
}) => {
  await page.route("**/+shadow/candidates?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("root/pypi");
    expect(url.searchParams.get("project")).toBe("example");
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
            decision: {
              state: "deny",
              rule: "blocked-item",
              reason: "item is blocked",
              evaluated_at_unix: 0,
              next_eligible_at_unix: null,
              fresh: true,
            },
          }),
          shadowCandidate({
            decision: {
              state: "wait",
              rule: "cooldown",
              reason: "rate limited",
              evaluated_at_unix: 0,
              next_eligible_at_unix: 60,
              fresh: true,
            },
          }),
          shadowCandidate({
            filename: "example-2.0-py3-none-any.whl",
            selected: true,
            reason: null,
          }),
          shadowCandidate({
            filename: "example-3.0-py3-none-any.whl",
            reason: "protected-name",
          }),
        ],
        next_cursor: null,
      }),
    });
  });
  await gotoShadow(page);
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("example");
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
  await expect(table).toContainText("Protected name");
  await expect(table).toContainText("1970-01-01T00:01:00Z");
  const undecided = table.locator("tbody tr", {
    hasText: "example-2.0-py3-none-any.whl",
  });
  await expect(undecided.locator("td").nth(1)).toHaveText("-");
});

test("shadow inspection escapes policy text and leaks no upstream url", async ({
  page,
}) => {
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
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ candidates: [candidate], next_cursor: null }),
    }),
  );
  await gotoShadow(page);
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("example");
  await page.locator(".policy-filters button[type='submit']").click();

  const row = page.locator(".shadow-inspection-table tbody tr");
  await expect(row).toContainText("Stale Denied");
  await expect(row.locator("td").nth(7)).toHaveText(candidate.decision.rule);
  await expect(row.locator("td").nth(8)).toHaveText(candidate.decision.reason);
  await expect(row.locator("img, script")).toHaveCount(0);
  expect(
    await page.evaluate(() => [
      window.shadowRuleExecuted,
      window.shadowReasonExecuted,
    ]),
  ).toEqual([undefined, undefined]);
  await expect(page.locator(".shadow-inspection-table")).not.toContainText(
    "http",
  );
});

test("shadow inspection distinguishes an empty result", async ({ page }) => {
  await page.route("**/+shadow/candidates?**", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ candidates: [], next_cursor: null }),
    }),
  );
  await gotoShadow(page);
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("missing");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("status")).toHaveText(
    "No candidates resolved for this repository and project.",
  );
});

test("shadow inspection pages a large project from the keyboard on a narrow screen", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 820 });
  await page.route("**/+shadow/candidates?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("root/pypi");
    expect(url.searchParams.get("project")).toBe("large-demo");
    const cursor = url.searchParams.get("cursor");
    const candidates = Array.from({ length: 100 }, (_, index) =>
      shadowCandidate({
        filename: `large-${String(index).padStart(3, "0")}.bin`,
        selected: index === 0,
        reason: index === 0 ? null : "precedence",
      }),
    );
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({
        candidates,
        next_cursor: cursor === null ? "page-2" : null,
      }),
    });
  });
  await gotoShadow(page);
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-user").focus();
  await page.keyboard.press("Tab");
  await expect(page.locator("#shadow-password")).toBeFocused();
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("large-demo");
  await page.locator(".policy-filters button[type='submit']").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".shadow-inspection-table tbody tr")).toHaveCount(
    100,
  );
  await expect(
    page.locator(".shadow-inspection-page .table-scroll"),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth > window.innerWidth + 1,
    ),
  ).toBe(false);

  await page.getByRole("button", { name: "Next" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("button", { name: "Next" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Previous" })).toBeEnabled();
  await page.getByRole("button", { name: "Previous" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".shadow-inspection-table tbody tr")).toHaveCount(
    100,
  );
  await expect(page.getByRole("button", { name: "Previous" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Next" })).toBeEnabled();
});

test("shadow inspection rejects a blank repository or project before any request", async ({
  page,
}) => {
  let requested = false;
  await page.route("**/+shadow/candidates?**", (route) => {
    requested = true;
    return route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ candidates: [], next_cursor: null }),
    });
  });
  await gotoShadow(page);
  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.evaluate(() => {
    for (const field of ["#shadow-repository", "#shadow-project"])
      document.querySelector(field).removeAttribute("required");
  });
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText(
    "Enter a repository and a project to inspect.",
  );
  expect(requested).toBe(false);
});

test("shadow inspection enforces administrator and repository-token boundaries", async ({
  page,
}) => {
  await gotoShadow(page);

  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("browser-admin-secret");
  await page.locator("#shadow-repository").fill("root/pypi");
  await page.locator("#shadow-project").fill("veloxdemo");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.locator(".policy-results")).not.toContainText(
    "Enter credentials",
  );
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.locator("#shadow-user").fill("__token__");
  await page.locator("#shadow-password").fill("playwright-reader");
  await page.locator("#shadow-repository").fill("internal");
  await page.locator("#shadow-project").fill("veloxdemo");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText(
    "This repository token cannot inspect shadowed candidates.",
  );

  await page.locator("#shadow-user").fill("administrator");
  await page.locator("#shadow-password").fill("wrong password");
  await page.locator(".policy-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText(
    "The username or password was not accepted.",
  );
});

test("project list links to project details", async ({ page }) => {
  await goto(page, "/browse?index=root%2Fpypi");
  const entry = page.locator(".links-list a", { hasText: "veloxdemo" });
  await expect(entry).toBeVisible();
  await expect(entry).toHaveAttribute("href", /project=veloxdemo/);
});

test("project page renders pypi.org-style metadata", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await expect(page.locator(".browse-head h1")).toContainText("veloxdemo");
  await expect(page.locator(".summary")).toContainText(
    "A demonstration package",
  );
  await expect(page.locator(".install code")).toContainText("uv pip install");
  await expect(page.locator(".description h2")).toContainText("Features");
  await expect(page.locator(".description strong")).toContainText("velox");
  await expect(page.locator(".browse-page")).toContainText("MIT");
  await expect(page.locator(".browse-page")).toContainText("requests>=2");
  await expect(page.locator(".browse-page")).toContainText(
    "Development Status",
  );
  await expect(
    page.locator(".browse-page a", { hasText: "Documentation" }),
  ).toBeVisible();
  const row = browseSection(page, "Files").locator("tbody tr", {
    hasText: "veloxdemo-1.0.0",
  });
  await expect(row).toContainText("1271");
});

test("project page groups hosted and upstream releases", async ({ page }) => {
  await goto(page, PROJECT_URL);
  await expect(browseSection(page, "Releases").locator("tbody tr")).toHaveCount(
    2,
  );
  const files = browseSection(page, "Files").locator("tbody tr");
  await expect(files).toHaveCount(2);
  await expect(files.filter({ hasText: "veloxdemo-1.0.0" })).toHaveCount(1);
  await expect(files.filter({ hasText: "veloxdemo-0.9" })).toHaveCount(1);
});

test("release links select one exact file set", async ({
  page,
}) => {
  await goto(page, PROJECT_URL);
  await browseSection(page, "Releases")
    .getByRole("link", { name: "0.9", exact: true })
    .click();

  await expect(page).toHaveURL(/version=0\.9/);
  const files = browseSection(page, "Files").locator("tbody tr");
  await expect(files.filter({ hasText: "veloxdemo-0.9" })).toHaveCount(1);
  await expect(files.filter({ hasText: "veloxdemo-1.0.0" })).toHaveCount(
    0,
  );
});

test("release navigation follows native keyboard order", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const releases = browseSection(page, "Releases");
  await releases.getByRole("link", { name: "1.0.0", exact: true }).focus();
  await page.keyboard.press("Tab");
  await expect(
    releases.getByRole("link", { name: "0.9", exact: true }),
  ).toBeFocused();
});

test("large histories group without rendering unrelated releases", async ({
  page,
}) => {
  await goto(page, "/browse?index=pypi&project=large-demo&version=99.0");
  const files = browseSection(page, "Files").locator("tbody tr");
  await expect(files).toHaveCount(20);
  expect((await files.allTextContents()).every((row) => row.includes("99.0"))).toBe(
    true,
  );
});

test("narrow project pages scroll only the file table", async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 800 });
  await goto(page, `${PROJECT_URL}&version=1.0.0`);

  const overflow = await page.evaluate(() => {
    const wrapper = [...document.querySelectorAll(".browse-section")]
      .find((section) => section.querySelector("h2")?.textContent === "Files")
      .querySelector(".table-scroll");
    return {
      page: document.documentElement.scrollWidth > window.innerWidth + 1,
      table: wrapper.scrollWidth > wrapper.clientWidth,
    };
  });
  expect(overflow).toEqual({ page: false, table: true });
});

test("project file search keeps URL history", async ({ page }) => {
  await goto(page, PROJECT_URL);
  const row = browseSection(page, "Files").locator("tbody tr", {
    hasText: "veloxdemo-1.0.0",
  });
  await expect(row).toBeVisible();
  await goto(page, `${PROJECT_URL}&filename=missing`);
  await expect(browseSection(page, "Files")).toContainText(
    "No files match this query.",
  );
  await expect(page).toHaveURL(/filename=missing/);

  await page.goBack();
  await expect(
    browseSection(page, "Files").locator("tbody tr", {
      hasText: "veloxdemo-1.0.0",
    }),
  ).toBeVisible();
  await expect(page).toHaveURL(
    /\/browse\?index=root%2Fpypi&project=veloxdemo$/,
  );

  await page.goForward();
  await expect(browseSection(page, "Files")).toContainText(
    "No files match this query.",
  );
});

test("project file regex reports invalid expressions", async ({ page }) => {
  await goto(
    page,
    `${PROJECT_URL}&filename=${encodeURIComponent("veloxdemo-1.*\\.whl")}&filename_match=regex`,
  );
  await expect(browseSection(page, "Files").locator("tbody tr", {
    hasText: "veloxdemo-1.0.0",
  })).toBeVisible();

  await goto(
    page,
    `${PROJECT_URL}&filename=${encodeURIComponent("[")}&filename_match=regex`,
  );
  await expect(page.locator(".error")).toContainText(/invalid regex/i);
});

test("archive browser lists members and shows file content", async ({
  page,
}) => {
  await goto(page, PROJECT_URL);
  await browseSection(page, "Files")
    .getByRole("link", { name: /veloxdemo-1\.0\.0.*\.whl/ })
    .click();
  const archive = browseSection(page, "Archive members");
  const metadataRow = archive.locator("a", {
    hasText: "METADATA",
  });
  await expect(metadataRow).toBeVisible();
  await metadataRow.click();
  await expect(browseSection(page, "Member").locator(".browse-content")).toContainText(
    "Metadata-Version: 2.1",
  );
  await page.goBack();
  await expect(
    browseSection(page, "Archive members").locator("a", {
      hasText: "__init__.py",
    }),
  ).toBeVisible();
});

test("project page exposes release and project actions", async ({
  page,
}) => {
  await goto(page, PROJECT_URL);
  const release = browseSection(page, "Releases")
    .locator("tbody tr")
    .filter({ hasText: "1.0.0" });
  await expect(release).toContainText("yank");
  await expect(release).toContainText("un-yank");
  await expect(release).toContainText("delete");
  await page.locator(".admin summary").click();
  await expect(
    page.getByRole("button", { name: "delete whole project" }),
  ).toBeVisible();
});

test("wrong token surfaces the auth failure", async ({ page }) => {
  const response = await page.request.put(
    "/root/pypi/veloxdemo/1.0.0/yank",
    {
      headers: {
        authorization: `Basic ${Buffer.from("__token__:wrong").toString("base64")}`,
      },
    },
  );
  expect(response.status()).toBe(401);
});

test("search surfaces provenance facets and the owning index", async ({
  page,
}) => {
  await goto(page, "/");
  await page.locator(".header-search input[name='q']").fill("veloxdemo");
  await page.locator(".suggestions a.all-results").click();
  await expect(
    page.locator("table.search-results th", { hasText: "Index" }),
  ).toBeVisible();
  await expect(
    page.locator("table.search-results th", { hasText: "Repository" }),
  ).toHaveCount(0);
  const row = page
    .locator("table.search-results tbody tr", { hasText: "veloxdemo" })
    .first();
  await expect(row).toBeVisible();
  await expect(row.locator("[class*='source-']")).toBeVisible();

  await navigateClient(page, "/search?q=veloxdemo&type=uploaded");
  const uploaded = page
    .locator("table.search-results tbody tr", { hasText: "veloxdemo" })
    .first();
  await expect(uploaded).toBeVisible();
  await expect(uploaded.locator(".badge.source-uploaded")).toBeVisible();

  const select = page.locator(".search-controls select[name='type']");
  await expect(page).toHaveURL(/type=uploaded/);
  await expect(select.locator("option")).toContainText([
    "All",
    "Uploaded",
    "Cached",
    "Override",
  ]);
});

test("usage stats show an empty virtual resource drill", async ({ browser }) => {
  const page = await operatorPage(browser);
  const detail = await page.request.get("/root/pypi/simple/veloxdemo/", {
    headers: { accept: "application/vnd.pypi.simple.v1+json" },
  });
  const files = (await detail.json()).files;
  await page.request.get(files[0].url);

  await goto(page, "/");
  const virtualIndex = page.locator(".card", { hasText: "root/pypi" });
  await expect(virtualIndex.locator(".card-usage")).toContainText("reads");
  await virtualIndex.locator(".card-usage a", { hasText: "usage" }).click();

  await expect(page.locator(".breadcrumb")).toContainText("root/pypi");
  await expect(page.locator(".stats-table tbody tr")).toHaveCount(0);
  await expect(
    page.getByText("Nothing recorded at this level yet.", { exact: true }),
  ).toBeVisible();
});

test("usage analytics enforces operator and repository-token boundaries", async ({
  page,
}) => {
  await goto(page, "/admin/analytics");
  await searchAnalytics(page);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(page.getByRole("status")).toBeVisible();

  await page.locator("#analytics-user").fill("__token__");
  await page.locator("#analytics-password").fill(TOKEN);
  await page.locator("#analytics-repository").fill("internal");
  await page.locator("#analytics-view").selectOption("sources");
  await page.locator(".analytics-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText(
    "This repository token cannot inspect usage analytics.",
  );

  await page.locator("#analytics-user").fill("administrator");
  await page.locator("#analytics-password").fill("wrong password");
  await page.locator("#analytics-view").selectOption("top");
  await page.locator("#analytics-repository").fill("");
  await page.locator(".analytics-filters button[type='submit']").click();
  await expect(page.getByRole("alert")).toHaveText(
    "The username or password was not accepted.",
  );
});
