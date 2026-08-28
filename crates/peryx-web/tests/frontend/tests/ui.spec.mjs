import { expect, test } from "@playwright/test";

import {
  boundaryCases,
  collectWasmCoverage,
  goto,
  operatorPage,
} from "../test-support.mjs";

const ADMIN_AUTH = `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`;

collectWasmCoverage(test);

test("navigation completes when the application hydrates", async ({ page }) => {
  const assetRequested = Promise.withResolvers();
  const releaseAsset = Promise.withResolvers();
  await page.route("**/mark.svg", async (route) => {
    assetRequested.resolve();
    await releaseAsset.promise;
    await route.continue();
  });
  const navigation = goto(page, "/");
  try {
    await assetRequested.promise;
    await navigation;
    expect(await page.locator("body").getAttribute("data-hydrated")).toBe(
      "true",
    );
  } finally {
    releaseAsset.resolve();
    await page.unrouteAll({ behavior: "wait" });
  }
});

function isPolicyResponse(response) {
  return new URL(response.url()).pathname.endsWith("/+policy/decisions");
}

async function submitPolicy(page) {
  const pending = page.waitForResponse(isPolicyResponse);
  await page.locator(".policy-filters button[type='submit']").click();
  return pending;
}

function policyDecision(state, fresh, resource = "blocked-item") {
  return {
    id: `decision-${state}`,
    repository: "internal",
    resource,
    group: "1.0",
    artifact: `${resource}-1.0.bin`,
    source: "fixture",
    action: "serve",
    state,
    rule: "blocked-item",
    reason: "item is blocked",
    evaluated_at_unix: 0,
    input_generation: { repository: 0, catalog: 0, policy: 0 },
    next_eligible_at_unix: null,
    fresh,
  };
}

for (const { label, path } of [
  { label: "Search", path: "/search?page_size=25" },
  { label: "Status", path: "/admin/status" },
  { label: "Dashboard", path: "/" },
]) {
  test(`header navigation reaches the ${label.toLowerCase()} route`, async ({
    page,
  }) => {
    await goto(page, "/");
    await page.locator(".nav-links a", { hasText: label }).click();
    await expect(page).toHaveURL(
      new RegExp(`${path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`),
    );
  });
}

for (const { label, href } of [
  { label: "Docs", href: /readthedocs/ },
  { label: "GitHub", href: /github\.com/ },
]) {
  test(`header ${label.toLowerCase()} link reaches its external site`, async ({
    page,
  }) => {
    await goto(page, "/");
    await expect(page.locator(".nav-links a", { hasText: label })).toHaveAttribute(
      "href",
      href,
    );
  });
}

test("unknown routes render the not-found fallback", async ({ page }) => {
  const response = await page.goto("/does-not-exist");
  expect(response.status()).toBe(404);
  await expect(page.locator("body")).toContainText("not found");
});

test("browse renders an empty result for a missing owner document", async ({
  page,
}) => {
  await page.route("**/+ui/browse?**", (route) => route.fulfill({ status: 404 }));
  await goto(page, "/");
  await page.evaluate(() => {
    const link = document.createElement("a");
    link.href = "/browse?index=missing";
    document.body.append(link);
    link.click();
  });
  await expect(page.getByText("Nothing matched this browse query.")).toBeVisible();
});

test("policy decision filters omit credentials and render fields", async ({
  page,
}) => {
  await page.route("**/+policy/decisions?**", async (route) => {
    expect(route.request().headers().authorization).toBe(
      `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`,
    );
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    expect(url.searchParams.get("state")).toBe("deny");
    expect(url.searchParams.get("rule")).toBe("blocked-item");
    expect(url.searchParams.get("source")).toBe("fixture");
    expect(url.searchParams.get("from")).toBe(
      String(Date.UTC(2024, 0, 2) / 1000),
    );
    expect(url.searchParams.get("to")).toBe(
      String(Date.UTC(2024, 0, 9) / 1000),
    );
    expect(url.searchParams.get("limit")).toBe("50");
    expect(url.search).not.toContain("browser-admin-secret");
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({
        decisions: [
          policyDecision("deny", false),
          {
            ...policyDecision("allow", true, "unversioned-item"),
            group: null,
            artifact: null,
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
  await page.locator("#policy-rule").fill("blocked-item");
  await page.locator("#policy-source").fill("fixture");
  await page.locator("#policy-from").fill("2024-01-02T00:00");
  await page.locator("#policy-to").fill("2024-01-09T00:00");
  await page.locator("#policy-limit").selectOption("50");
  await submitPolicy(page);

  const table = page.locator(".policy-decisions-table");
  await expect(table).toContainText("Stale Denied");
  await expect(table).toContainText("blocked-item");
  await expect(table).toContainText("1970-01-01T00:00:00Z");
  await expect(table.getByRole("columnheader")).toHaveText([
    "Outcome",
    "Repository",
    "Resource",
    "Group",
    "Artifact",
    "Source",
    "Action",
    "Rule",
    "Reason",
    "Evaluated (UTC)",
    "Next eligible (UTC)",
  ]);
  await expect(table.locator("tbody td")).toHaveText([
    "Stale Denied",
    "internal",
    "blocked-item",
    "1.0",
    "blocked-item-1.0.bin",
    "fixture",
    "serve",
    "blocked-item",
    "item is blocked",
    "1970-01-01T00:00:00Z",
    "-",
    "Allowed",
    "internal",
    "unversioned-item",
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
  await expect(page.locator("#policy-password")).toHaveAttribute(
    "autocomplete",
    "off",
  );
  expect(
    await page.evaluate(() =>
      [localStorage, sessionStorage].flatMap((storage) =>
        Array.from({ length: storage.length }, (_, index) =>
          storage.getItem(storage.key(index)),
        ),
      ),
    ),
  ).not.toContain("browser-admin-secret");
});

test("policy decision rule text is rendered without markup execution", async ({
  page,
}) => {
  const decision = policyDecision("deny", true);
  decision.rule =
    '<img src="missing" onerror="window.policyRuleExecuted=true">';
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
  await submitPolicy(page);

  const row = page.locator(".policy-decisions-table tbody tr");
  await expect(row.locator("td").nth(7)).toHaveText(decision.rule);
  await expect(row.locator("td").nth(8)).toHaveText(decision.reason);
  await expect(row.locator("img, script")).toHaveCount(0);
  expect(
    await page.evaluate(() => [
      window.policyRuleExecuted,
      window.policyReasonExecuted,
    ]),
  ).toEqual([undefined, undefined]);
});

test("policy decision pagination keeps filters and works from the keyboard", async ({
  page,
}) => {
  let requests = 0;
  await page.route("**/+policy/decisions?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    expect(url.searchParams.get("state")).toBe("deny");
    expect(url.searchParams.get("rule")).toBe("blocked-item");
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
  await page.locator("#policy-rule").fill("blocked-item");
  await page.locator(".policy-filters button[type='submit']").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".policy-decisions-table")).toContainText("Denied");

  await page.getByRole("button", { name: "Next" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".policy-decisions-table")).toContainText(
    "Waiting",
  );
  await page.getByRole("button", { name: "Next" }).dispatchEvent("click");
  expect(requests).toBe(2);
  await page.getByRole("button", { name: "Previous" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".policy-decisions-table")).toContainText("Denied");
  expect(requests).toBe(3);
});

test("policy decision filters reject invalid local dates", async ({ page }) => {
  await goto(page, "/admin/policy-decisions");
  await page.locator("#policy-from").evaluate((input) => {
    input.type = "text";
    input.value = "invalid";
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
  });
  await page.locator(".policy-filters").dispatchEvent("submit");
  await expect(page.getByRole("alert")).toHaveText(
    "Invalid UTC date and time: invalid",
  );
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
  await submitPolicy(page);
  await expect(page.getByRole("status")).toHaveText(
    "No policy decisions matched these filters.",
  );
});

boundaryCases(test, {
  title: "policy decision view",
  cases: [
    {
      label: "invalid filter",
      response: { status: 400, body: "secret-package must stay hidden" },
      expectedAlert: "One or more policy decision filters are invalid.",
      excludedText: "secret-package",
    },
    {
      label: "invalid local credential",
      response: { status: 401, body: "secret-package must stay hidden" },
      expectedAlert: "The username or password was not accepted.",
      excludedText: "secret-package",
    },
    {
      label: "repository token boundary",
      response: { status: 403, body: "secret-package must stay hidden" },
      expectedAlert: "This repository token cannot inspect policy decisions.",
      excludedText: "secret-package",
    },
    {
      label: "repository boundary",
      response: { status: 404, body: "secret-package must stay hidden" },
      expectedAlert:
        "The repository was not found or is not available to this user.",
      excludedText: "secret-package",
    },
    {
      label: "service failure",
      response: { status: 503, body: "secret-package must stay hidden" },
      expectedAlert: "The policy decision service is unavailable.",
      excludedText: "secret-package",
    },
    {
      label: "malformed success data",
      response: { status: 200, body: "secret-package" },
      expectedAlert: "The policy decision service returned invalid data.",
      excludedText: "secret-package",
    },
    {
      label: "a network failure",
      expectedAlert: "The policy decision service could not be reached.",
    },
  ],
  setupRoute: (page, boundary) =>
    page.route("**/+policy/decisions?**", (route) =>
      boundary.response
        ? route.fulfill(boundary.response)
        : route.abort("connectionfailed"),
    ),
  navigate: (page) => goto(page, "/admin/policy-decisions"),
  action: async (page) => {
    await page.locator("#policy-user").fill("administrator");
    await page.locator("#policy-password").fill("browser-admin-secret");
    await page.locator(".policy-filters button[type='submit']").click();
  },
});

test("every page sets the app favicon", async ({ page }) => {
  await goto(page, "/admin/status");
  await expect(page.locator("head link[rel='icon']")).toHaveAttribute(
    "href",
    "/favicon.svg",
  );
  const response = await page.request.get("/favicon.svg");
  expect(response.headers()["content-type"]).toContain("image/svg+xml");
  const svg = await response.text();
  expect(svg).toContain("512 512");
  expect(svg).toContain("Peryx app icon");
});

test("theme toggle switches and survives a reload", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".theme-toggle").click();
  const forced = await page.evaluate(
    () => document.documentElement.dataset.theme,
  );
  expect(["light", "dark"]).toContain(forced);
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", forced);
  await page.locator(".theme-toggle").click();
});

test("search renders typed neutral results and opens actions", async ({ page }) => {
  await page.route("**/+search?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("page_size")).toBe("25");
    await route.fulfill({ json: searchPage() });
  });
  await goto(page, "/");
  await page.locator('.nav-links a[href="/search?page_size=25"]').click();

  const table = page.locator("table.search-results");
  await expect(table.getByRole("columnheader")).toHaveText([
    "Name",
    "Type",
    "Normalized",
    "Source",
    "Availability",
    "Index",
    "Summary",
  ]);
  await expect(table.locator("tbody td")).toHaveText([
    "Example record",
    "record",
    "example-record",
    "Uploaded",
    "local",
    "catalog/example",
    "Neutral fixture summary",
    "Example override",
    "record",
    "example-override",
    "Override",
    "remote",
    "catalog/example",
    "",
  ]);
  await expect(
    table.getByRole("link", { name: "Example record" }),
  ).toHaveAttribute("href", "/browse?index=catalog%2Fexample");
  await page.route("**/+ui/browse?**", (route) =>
    route.fulfill({
      json: {
        breadcrumbs: [
          {
            label: "Usage",
            href: "/stats?index=root%2Fcache&resource=artifact",
          },
        ],
        title: "Managed artifact",
        subtitle: null,
        summary: null,
        command: null,
        badges: [],
        sections: [],
        actions: [
          { label: "Put", method: "put", endpoint: "/admin/put" },
          { label: "Post", method: "post", endpoint: "/admin/post" },
          {
            label: "Delete",
            method: "delete",
            endpoint: "/admin/delete",
            destructive: true,
          },
        ],
      },
    }),
  );
  const actionRoutes = [];
  const pendingActionRoutes = [];
  await page.route("**/admin/**", (route) => {
    pendingActionRoutes.shift()(route);
  });
  await table.getByRole("link", { name: "Example record" }).click();
  await page.getByText("Manage", { exact: true }).click();
  await page.getByPlaceholder("Username").fill("administrator");
  await page.getByPlaceholder("Password").fill("browser-admin-secret");
  for (const label of ["Put", "Post", "Delete"]) {
    const actionRoute = new Promise((resolve) =>
      pendingActionRoutes.push(resolve),
    );
    await page.getByRole("button", { name: label, exact: true }).click();
    actionRoutes.push(await actionRoute);
  }
  await actionRoutes[0].fulfill({ status: 200, body: "ok" });
  await actionRoutes[1].fulfill({ status: 200, body: "ok" });
  await actionRoutes[2].abort();
  expect(
    actionRoutes.map((route) => [
      route.request().method(),
      route.request().headers().authorization,
    ]),
  ).toEqual(
    ["PUT", "POST", "DELETE"].map((method) => [method, ADMIN_AUTH]),
  );
  await page.route("**/+stats**", (route) => route.fulfill({ json: {} }));
  await page.locator(".breadcrumb").getByRole("link", { name: "Usage" }).click();
  await expect(page.locator(".breadcrumb")).toContainText("artifact");
});

function searchPage() {
  return {
    query: "example",
    type: "all",
    availability: "all",
    page: 1,
    page_size: 25,
    total: 2,
    results: [
      {
        display_label: "Example record",
        resource_key: "example-record",
        route: "catalog/example",
        index: "catalog/example",
        ecosystem: "example",
        type_label: "record",
        type: "uploaded",
        available: true,
        summary: "Neutral fixture summary",
      },
      {
        display_label: "Example override",
        resource_key: "example-override",
        route: "catalog/example",
        index: "catalog/example",
        ecosystem: "example",
        type_label: "record",
        type: "override",
        available: false,
        summary: null,
      },
    ],
  };
}

boundaryCases(test, {
  title: "search",
  cases: [
    {
      label: "invalid input",
      response: { status: 400 },
      expectedAlert: "The search request was invalid.",
    },
    {
      label: "denied access",
      response: { status: 403 },
      expectedAlert: "You do not have access to search this index.",
    },
    {
      label: "a server failure",
      response: { status: 500 },
      expectedAlert: "Search is unavailable.",
    },
    {
      label: "a malformed success",
      response: { status: 200, body: "{" },
      expectedAlert: "Search returned invalid data.",
    },
    { label: "a network failure", expectedAlert: "Search could not be reached." },
  ],
  setupRoute: (page, boundary) =>
    page.route("**/+search?**", (route) =>
      boundary.response ? route.fulfill(boundary.response) : route.abort(),
    ),
  navigate: async (page) => {
    await goto(page, "/");
    await page.locator('.nav-links a[href="/search?page_size=25"]').click();
  },
});

function usageEnvelope(
  rowsKey,
  rows,
  { nextCursor = null, clamped = false, retainedFromDay = null } = {},
) {
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

async function searchAnalytics(
  page,
  { user = "administrator", password = "browser-admin-secret" } = {},
) {
  await page.locator("#analytics-user").fill(user);
  await page.locator("#analytics-password").fill(password);
  await page.locator(".analytics-filters button[type='submit']").click();
}

test("usage analytics maps filters to the API, keeps credentials out of the URL, and renders ties", async ({
  page,
}) => {
  await page.route("**/+analytics/top-resources?**", async (route) => {
    expect(route.request().headers().authorization).toBe(
      `Basic ${Buffer.from("administrator:browser-admin-secret").toString("base64")}`,
    );
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    expect(url.searchParams.get("from")).toBe(
      String(Date.UTC(2024, 0, 2) / 1000),
    );
    expect(url.searchParams.get("to")).toBe(
      String(Date.UTC(2024, 0, 9) / 1000),
    );
    expect(url.searchParams.get("limit")).toBe("50");
    expect(url.search).not.toContain("browser-admin-secret");
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(
        usageEnvelope("resources", [
          {
            repository: "internal",
            resource: "alpha",
            reads: 40,
            bytes: 2048,
          },
          {
            repository: "internal",
            resource: "beta",
            reads: 40,
            bytes: 1024,
          },
        ]),
      ),
    });
  });
  await goto(page, "/admin/analytics");
  await page.locator("#analytics-repository").fill("internal");
  await page.locator("#analytics-from").fill("2024-01-02");
  await page.locator("#analytics-to").fill("2024-01-09");
  await page.locator("#analytics-limit").selectOption("50");
  await searchAnalytics(page);

  const table = page.locator(".usage-top-table");
  await expect(table.getByRole("columnheader")).toHaveText([
    "Repository",
    "Resource",
    "Reads",
    "Bytes",
  ]);
  await expect(table.locator("tbody td")).toHaveText([
    "internal",
    "alpha",
    "40",
    "2.0 kB",
    "internal",
    "beta",
    "40",
    "1.0 kB",
  ]);
  await expect(page.locator(".usage-interval")).toContainText("UTC, inclusive");
  await expect(page.getByRole("status")).toContainText("Loaded 2 rows.");
  await expect(page).toHaveURL(/\/admin\/analytics$/);
  await expect(page.locator("#analytics-password")).toHaveAttribute(
    "autocomplete",
    "off",
  );
  expect(
    await page.evaluate(() =>
      [localStorage, sessionStorage].flatMap((storage) =>
        Array.from({ length: storage.length }, (_, index) =>
          storage.getItem(storage.key(index)),
        ),
      ),
    ),
  ).not.toContain("browser-admin-secret");
});

for (const { view, endpoint, rowsKey, row, headers, caption, cells } of [
  {
    view: "groups",
    endpoint: "groups",
    rowsKey: "groups",
    row: {
      repository: "internal",
      resource: "alpha",
      group: null,
      reads: 3,
      bytes: 90,
    },
    headers: ["Repository", "Resource", "Group", "Reads", "Bytes"],
    caption: "1 groups",
    cells: ["internal", "alpha", "-", "3", "90.0 B"],
  },
  {
    view: "sources",
    endpoint: "sources",
    rowsKey: "sources",
    row: {
      repository: "internal",
      resource: "alpha",
      source: null,
      reads: 3,
      bytes: 90,
    },
    headers: ["Repository", "Resource", "Source", "Reads", "Bytes"],
    caption: "1 source rows",
    cells: ["internal", "alpha", "local store", "3", "90.0 B"],
  },
  {
    view: "unused",
    endpoint: "unused",
    rowsKey: "unused",
    row: { repository: "internal", resource: "gamma", lifetime_reads: 12 },
    headers: ["Repository", "Resource", "Lifetime reads"],
    caption: "1 unused resources",
    cells: ["internal", "gamma", "12"],
  },
  {
    view: "timeline",
    endpoint: "timeline",
    rowsKey: "buckets",
    row: {
      day: 19000,
      start_unix: 19000 * 86400,
      end_unix: 19001 * 86400,
      reads: 5,
      bytes: 500,
    },
    headers: ["Start (UTC)", "End (UTC)", "Reads", "Bytes"],
    caption: "1 daily buckets",
    cells: [
      "2022-01-08T00:00:00Z",
      "2022-01-09T00:00:00Z",
      "5",
      "500.0 B",
    ],
  },
]) {
  test(`usage analytics renders the ${view} view with its own columns`, async ({
    page,
  }) => {
    await page.route(`**/+analytics/${endpoint}?**`, (route) =>
      route.fulfill({
        contentType: "application/json",
        body: JSON.stringify(usageEnvelope(rowsKey, [row])),
      }),
    );
    await goto(page, "/admin/analytics");
    await page.locator("#analytics-view").selectOption(view);
    await searchAnalytics(page);
    const table = page.locator(".usage-table");
    await expect(table.getByRole("columnheader")).toHaveText(headers);
    await expect(table.locator("caption")).toHaveText(caption);
    await expect(table.locator("tbody td")).toHaveText(cells);
  });
}

test("usage analytics distinguishes an empty window from one clamped to retention", async ({
  page,
}) => {
  let clamped = false;
  await page.route("**/+analytics/top-resources?**", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(
        usageEnvelope("resources", [], {
          clamped,
          retainedFromDay: clamped ? 19010 : null,
        }),
      ),
    }),
  );
  await goto(page, "/admin/analytics");
  await searchAnalytics(page);
  await expect(page.getByRole("status")).toContainText("No usage recorded");
  await expect(page.getByRole("note")).toHaveCount(0);

  clamped = true;
  await searchAnalytics(page);
  await expect(page.getByRole("note")).toContainText(
    "Window clamped to retention",
  );
  await expect(page.getByRole("note")).toContainText("aged out");
  await expect(page.getByRole("status")).toContainText("No usage recorded");
});

test("usage analytics pagination keeps the view and filters and works from the keyboard", async ({
  page,
}) => {
  let requests = 0;
  await page.route("**/+analytics/groups?**", async (route) => {
    const url = new URL(route.request().url());
    expect(url.searchParams.get("repository")).toBe("internal");
    const cursor = url.searchParams.get("cursor");
    requests += 1;
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(
        usageEnvelope(
          "groups",
          [
            {
              repository: "internal",
              resource: cursor === null ? "first" : "second",
              group: "1.0",
              reads: 1,
              bytes: 2,
            },
          ],
          { nextCursor: cursor === null ? "page-2" : null },
        ),
      ),
    });
  });
  await goto(page, "/admin/analytics");
  await page.locator("#analytics-view").selectOption("groups");
  await page.locator("#analytics-repository").fill("internal");
  await searchAnalytics(page);
  await expect(page.locator(".usage-table")).toContainText("first");

  await page.getByRole("button", { name: "Next" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".usage-table")).toContainText("second");
  await page.getByRole("button", { name: "Next" }).dispatchEvent("click");
  expect(requests).toBe(2);
  await page.getByRole("button", { name: "Previous" }).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".usage-table")).toContainText("first");
  expect(requests).toBe(3);
});

test("usage analytics rejects invalid local dates", async ({ page }) => {
  await goto(page, "/admin/analytics");
  await page.locator("#analytics-from").evaluate((input) => {
    input.type = "text";
  });
  await page.locator("#analytics-from").fill("invalid");
  await searchAnalytics(page);
  await expect(page.getByRole("alert")).toHaveText("Invalid UTC date: invalid");
});

boundaryCases(test, {
  title: "usage analytics",
  cases: [
    {
      label: "invalid input",
      response: { status: 400, body: "secret-package must stay hidden" },
      expectedAlert: "One or more analytics filters are invalid.",
      excludedText: "secret-package",
    },
    {
      label: "invalid local credential",
      response: { status: 401, body: "secret-package must stay hidden" },
      expectedAlert: "The username or password was not accepted.",
      excludedText: "secret-package",
    },
    {
      label: "denied access",
      response: { status: 403, body: "secret-package must stay hidden" },
      expectedAlert: "This repository token cannot inspect usage analytics.",
      excludedText: "secret-package",
    },
    {
      label: "repository boundary",
      response: { status: 404, body: "secret-package must stay hidden" },
      expectedAlert:
        "The repository was not found or is not available to this user.",
      excludedText: "secret-package",
    },
    {
      label: "a server failure",
      response: { status: 503, body: "secret-package must stay hidden" },
      expectedAlert: "The analytics service is unavailable.",
      excludedText: "secret-package",
    },
    {
      label: "a malformed success",
      response: { status: 200, body: "secret-package" },
      expectedAlert: "The analytics service returned invalid data.",
      excludedText: "secret-package",
    },
    {
      label: "a network failure",
      expectedAlert: "The analytics service could not be reached.",
    },
  ],
  setupRoute: (page, boundary) =>
    page.route("**/+analytics/top-resources?**", (route) =>
      boundary.response
        ? route.fulfill(boundary.response)
        : route.abort("connectionfailed"),
    ),
  navigate: (page) => goto(page, "/admin/analytics"),
  action: searchAnalytics,
});

test("usage analytics nav link reaches the route", async ({ page }) => {
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Usage" }).click();
  await expect(page).toHaveURL(/\/admin\/analytics$/);
  await expect(
    page.locator("h1", { hasText: "Usage analytics" }),
  ).toBeVisible();
});

const topologySnapshot = {
  mode: "dc",
  group: "east",
  captured_at: 1_800_000_000,
  node_count: 3,
  local: { role: "writer", liveness: "live", frontier: 42 },
  nodes: [
    {
      node: "writer-a",
      dc: "east-1",
      role: "writer",
      local: true,
      liveness: "live",
      frontier: 42,
      address: "writer-a.internal:8443",
    },
    {
      node: "replica-b",
      dc: "east-2",
      role: "replica",
      local: false,
      liveness: "unknown",
      address: "replica-b.internal:8443",
    },
    {
      node: "replica-c",
      dc: "east-3",
      role: "replica",
      local: false,
      liveness: "unready",
      address: "replica-c.internal:8443",
    },
  ],
};

test("availability topology renders roster health and filters by role", async ({
  page,
}) => {
  await page.route("**/+availability/topology", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(topologySnapshot),
    }),
  );
  await page.route("**/+availability/topology/stream", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: `id: 1\nevent: topology\ndata: ${JSON.stringify(topologySnapshot)}\n\n`,
    }),
  );
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Topology" }).click();
  await expect(page).toHaveURL(/\/admin\/topology$/);
  await expect(
    page.locator("h1", { hasText: "Availability topology" }),
  ).toBeVisible();
  await expect(
    page.locator(".ops-title .badge", { hasText: "feed:" }),
  ).toBeVisible();

  const rows = page.locator(".topology-table tbody tr");
  await expect(rows.locator("td")).toHaveText([
    "writer-a this node",
    "east-1",
    "Writer",
    "Live",
    "42",
    "writer-a.internal:8443",
    "replica-b",
    "east-2",
    "Replica",
    "Unknown",
    "-",
    "replica-b.internal:8443",
    "replica-c",
    "east-3",
    "Replica",
    "Unready",
    "-",
    "replica-c.internal:8443",
  ]);

  await page.locator("#topology-role").selectOption("replica");
  await expect(rows.locator("td")).toHaveText([
    "replica-b",
    "east-2",
    "Replica",
    "Unknown",
    "-",
    "replica-b.internal:8443",
    "replica-c",
    "east-3",
    "Replica",
    "Unready",
    "-",
    "replica-c.internal:8443",
  ]);
  await expect(page.locator(".result-count")).toHaveText(
    "Showing 2 of 3 roster nodes.",
  );

  await page.locator("#topology-role").selectOption("writer");
  await expect(rows.locator("td")).toHaveText([
    "writer-a this node",
    "east-1",
    "Writer",
    "Live",
    "42",
    "writer-a.internal:8443",
  ]);

  await page.locator("#topology-role").selectOption("all");
  await expect(rows).toHaveCount(3);
  await page.locator(".nav-links a", { hasText: "Dashboard" }).click();
  await expect(page).toHaveURL(/\/$/);
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
        this.listeners.get("topology")?.(
          new MessageEvent("topology", { data }),
        );
      }

      drop() {
        this.readyState = MockEventSource.CONNECTING;
        this.onerror?.(new Event("error"));
      }

      offline() {
        this.readyState = MockEventSource.CLOSED;
        this.onerror?.(new Event("error"));
      }
    }
    window.EventSource = MockEventSource;
  });
}

async function openTopology(page) {
  await page.route("**/+availability/topology", (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(topologySnapshot),
    }),
  );
  await goto(page, "/");
  await page.locator(".nav-links a", { hasText: "Topology" }).click();
  await expect(page).toHaveURL(/\/admin\/topology$/);
  await page.waitForFunction(() => window.__topologyStream !== undefined);
  return page.locator(".ops-title .badge", { hasText: "feed:" });
}

for (const { title, states } of [
  {
    title: "topology feed remains reconnecting until the stream opens",
    states: [
      { events: [], expectedBadge: "feed: Reconnecting" },
      { events: [["open"]], expectedBadge: "feed: Live" },
    ],
  },
  {
    title: "topology feed marks undecodable data as stale",
    states: [
      {
        events: [["open"], ["message", "not a snapshot"]],
        expectedBadge: "feed: Stale",
      },
    ],
  },
  {
    title: "topology feed reconnects after a drop",
    states: [
      {
        events: [["open"], ["drop"]],
        expectedBadge: "feed: Reconnecting",
      },
      { events: [["open"]], expectedBadge: "feed: Live" },
    ],
  },
  {
    title: "topology feed ignores non-text events",
    states: [
      {
        events: [["message", { invalid: true }]],
        expectedBadge: "feed: Reconnecting",
      },
    ],
  },
  {
    title: "topology feed reports a closed stream as offline",
    states: [{ events: [["offline"]], expectedBadge: "feed: Offline" }],
  },
  {
    title: "topology feed cancels a pending reconnect after recovery",
    states: [
      {
        events: [["drop"], ["open"]],
        expectedBadge: "feed: Live",
      },
    ],
  },
]) {
  test(title, async ({ page }) => {
    await mockTopologyStream(page);
    const badge = await openTopology(page);
    for (const { events, expectedBadge } of states) {
      await page.evaluate((streamEvents) => {
        for (const [event, ...args] of streamEvents) {
          window.__topologyStream[event](...args);
        }
      }, events);
      await expect(badge).toHaveText(expectedBadge);
    }
    await page.locator(".nav-links a", { hasText: "Dashboard" }).click();
    await expect(page).toHaveURL(/\/$/);
  });
}

test("topology applies a streamed snapshot", async ({ page }) => {
  await mockTopologyStream(page);
  const badge = await openTopology(page);
  await page.evaluate((snapshot) => {
    window.__topologyStream.message(JSON.stringify(snapshot));
  }, { ...topologySnapshot, group: "streamed-group" });

  await expect(badge).toHaveText("feed: Live");
  await expect(page.locator(".topology-page")).toContainText("streamed-group");
});

test("topology reports offline when the browser cannot open a stream", async ({
  page,
}) => {
  await page.addInitScript(() => {
    window.EventSource = class {
      constructor() {
        throw new Error("EventSource unavailable");
      }
    };
  });
  await page.route("**/+availability/topology", (route) =>
    route.fulfill({ json: topologySnapshot }),
  );
  await goto(page, "/admin/topology");
  await expect(
    page.locator(".ops-title .badge", { hasText: "feed:" }),
  ).toHaveText("feed: Offline");
});

test("topology reports a missing snapshot", async ({ page }) => {
  await page.route("**/+availability/topology", (route) =>
    route.fulfill({ status: 404 }),
  );
  await goto(page, "/");
  await page.locator('.nav-links a[href="/admin/topology"]').click();
  await expect(page.getByRole("alert")).toHaveText(
    "The availability topology could not be reached.",
  );
});

test("operations render rows and paginate in the hydrated router", async ({ page }) => {
  await operatorPage(page);
  const cursors = [];
  await page.route("**/+availability/operations*", async (route) => {
    const cursor = new URL(route.request().url()).searchParams.get("cursor");
    cursors.push(cursor);
    await route.fulfill({
      json: {
        captured_at: 1_767_225_600,
        health: { pending: 1, published: 2, failed: 3, expired: 4, total: 10 },
        rows: cursor
          ? []
          : [
              {
                operation: "op-published",
                status: "published",
                updated_at: 1_767_225_600,
              },
              {
                operation: "op-pending",
                status: "pending",
                updated_at: 1_767_225_601,
                expires_at: 1_767_225_700,
              },
              {
                operation: "op-failed",
                status: "failed",
                updated_at: 1_767_225_602,
              },
              {
                operation: "op-expired",
                status: "expired",
                updated_at: 1_767_225_603,
              },
            ],
        next_cursor: cursor ? null : "op-next",
      },
    });
  });
  await goto(page, "/");
  await page.locator('a[href="/admin/operations"]').click();
  await expect(
    page.getByRole("heading", { name: "Pending operations" }),
  ).toBeVisible();
  await expect(page.locator(".operations-table tbody td")).toHaveText([
    "op-published",
    "Published",
    "2026-01-01T00:00:00Z",
    "-",
    "op-pending",
    "Pending",
    "2026-01-01T00:00:01Z",
    "2026-01-01T00:01:40Z",
    "op-failed",
    "Failed",
    "2026-01-01T00:00:02Z",
    "-",
    "op-expired",
    "Expired",
    "2026-01-01T00:00:03Z",
    "-",
  ]);
  await page.getByRole("button", { name: "Next page" }).click();
  await expect(page.getByText("No operations are recorded yet.")).toBeVisible();
  expect(cursors).toEqual([null, "op-next"]);
});

boundaryCases(test, {
  title: "operations",
  cases: [
    {
      label: "invalid input",
      response: { status: 400 },
      expectedAlert: "The operation page request was invalid.",
    },
    {
      label: "denied access",
      response: { status: 403 },
      expectedAlert: "You do not have access to operation health.",
    },
    {
      label: "a server failure",
      response: { status: 500 },
      expectedAlert: "Operation health is unavailable.",
    },
    {
      label: "a malformed success",
      response: { status: 200, body: "{" },
      expectedAlert: "Operation health returned invalid data.",
    },
    {
      label: "a network failure",
      expectedAlert: "Operation health could not be reached.",
    },
  ],
  setupRoute: (page, boundary) =>
    page.route("**/+availability/operations*", (route) =>
      boundary.response ? route.fulfill(boundary.response) : route.abort(),
    ),
  navigate: async (page) => {
    await goto(page, "/");
    await page.locator('a[href="/admin/operations"]').click();
  },
  createPage: ({ page }) => operatorPage(page),
});

test("placements render rows, datacenters, and pagination", async ({ page }) => {
  await operatorPage(page);
  const cursors = [];
  await page.route("**/+availability/placements**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.includes("/sha256:")) {
      await route.fulfill({
        json: {
          digest: "sha256:fixture",
          datacenters: [
            {
              data_center: "dc-verified",
              status: "verified",
              size: 42,
              updated_at: 1_767_225_600,
            },
            {
              data_center: "dc-pending",
              status: "pending",
              updated_at: 1_767_225_601,
            },
            {
              data_center: "dc-failed",
              status: "failed",
              updated_at: 1_767_225_602,
            },
            {
              data_center: "dc-revoked",
              status: "revoked",
              updated_at: 1_767_225_603,
            },
          ],
        },
      });
      return;
    }
    const cursor = url.searchParams.get("cursor");
    cursors.push(cursor);
    await route.fulfill({
      json: {
        captured_at: 1_767_225_600,
        health: { local: 1, remote_only: 1, unavailable: 1, total: 3 },
        rows: cursor
          ? []
          : [
              {
                digest:
                  "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                source: "hosted",
                availability: "local",
              },
              {
                digest:
                  "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                source: "proxy",
                availability: "remote_only",
              },
              {
                digest:
                  "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                source: "generated",
                availability: "unavailable",
              },
            ],
        next_cursor: cursor ? null : "sha256:next",
      },
    });
  });
  await goto(page, "/");
  await page.locator('a[href="/admin/placements"]').click();
  await expect(page.locator(".placement-table tbody td")).toHaveText([
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "hosted",
    "local",
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "proxy",
    "remote only",
    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    "generated",
    "unavailable",
  ]);
  await page
    .getByRole("button", {
      name: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    })
    .click();
  await expect(page.getByText("dc-verified")).toBeVisible();
  await expect(page.getByText("Revoked", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Next page" }).click();
  await expect(
    page.getByText("No artifact placements are recorded yet."),
  ).toBeVisible();
  expect(cursors).toEqual([null, "sha256:next"]);
});

for (const { title, response, expected, role = "alert" } of [
  {
    title: "empty placement detail",
    response: {
      status: 200,
      json: { digest: "sha256:fixture", datacenters: [] },
    },
    expected: "No datacenter holds sha256:fixture yet.",
    role: "status",
  },
  {
    title: "invalid placement digest",
    response: { status: 400 },
    expected: "That is not a valid artifact digest.",
  },
  {
    title: "denied placement detail",
    response: { status: 403 },
    expected: "You do not have access to blob placement.",
  },
  {
    title: "unavailable placement detail",
    response: { status: 500 },
    expected: "Blob placement is unavailable.",
  },
]) {
  test(`placements report ${title}`, async ({ page }) => {
    await operatorPage(page);
    await page.route("**/+availability/placements**", (route) => {
      const url = new URL(route.request().url());
      if (url.pathname.includes("/sha256:")) {
        return route.fulfill(response);
      }
      return route.fulfill({
        json: {
          captured_at: 1_767_225_600,
          health: { local: 1, remote_only: 0, unavailable: 0, total: 1 },
          rows: [
            {
              digest: "sha256:fixture",
              source: "hosted",
              availability: "local",
            },
          ],
          next_cursor: null,
        },
      });
    });
    await goto(page, "/");
    await page.locator('a[href="/admin/placements"]').click();
    await page.getByRole("button", { name: "sha256:fixture" }).click();
    await expect(page.getByRole(role).filter({ hasText: expected })).toHaveText(
      expected,
    );
  });
}

boundaryCases(test, {
  title: "placements",
  cases: [
    {
      label: "invalid input",
      response: { status: 400 },
      expectedAlert: "The placement page request was invalid.",
    },
    {
      label: "denied access",
      response: { status: 401 },
      expectedAlert: "You do not have access to placement health.",
    },
    {
      label: "a server failure",
      response: { status: 500 },
      expectedAlert: "Placement health is unavailable.",
    },
    {
      label: "a malformed success",
      response: { status: 200, body: "{" },
      expectedAlert: "Placement health returned invalid data.",
    },
  ],
  setupRoute: (page, boundary) =>
    page.route("**/+availability/placements*", (route) =>
      route.fulfill(boundary.response),
    ),
  navigate: async (page) => {
    await goto(page, "/");
    await page.locator('a[href="/admin/placements"]').click();
  },
  createPage: ({ page }) => operatorPage(page),
});

test("placements report overview and detail boundary failures", async ({ page }) => {
  await operatorPage(page);
  await page.route("**/+availability/placements**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.includes("/sha256:")) {
      await route.abort();
    } else {
      await route.fulfill({
        json: {
          captured_at: 1_767_225_600,
          health: { local: 1, remote_only: 0, unavailable: 0, total: 1 },
          rows: [
            {
              digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              source: "hosted",
              availability: "local",
            },
          ],
          next_cursor: null,
        },
      });
    }
  });
  await goto(page, "/");
  await page.locator('a[href="/admin/placements"]').click();
  await page
    .getByRole("button", {
      name: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    })
    .click();
  await expect(page.getByRole("alert")).toHaveText(
    "Blob placement could not be reached.",
  );
});

test("trash filters, authenticates, renders, and paginates", async ({
  page,
}) => {
  const cursors = [];
  await page.route("**/+trash?*", async (route) => {
    expect(route.request().headers().authorization).toBe(ADMIN_AUTH);
    const url = new URL(route.request().url());
    const cursor = url.searchParams.get("cursor");
    cursors.push(cursor);
    expect(url.searchParams.get("repository")).toBe("hosted");
    expect(url.searchParams.get("ecosystem")).toBe("fixture");
    expect(url.searchParams.get("state")).toBe("restorable");
    expect(url.searchParams.get("limit")).toBe("50");
    await route.fulfill({
      json: {
        trash: cursor
          ? []
          : [
              {
                ecosystem: "fixture",
                repository: "hosted",
                name: "artifact.bin",
                reference: "1.0",
                digest: "sha256:fixture",
                reason: "replaced",
                actor: "User",
                deleted_at_unix: 1_767_225_600,
                deadline_unix: 1_767_312_000,
                state: "restorable",
                restorable: true,
              },
              {
                ecosystem: "fixture",
                repository: "hosted",
                name: "unknown.bin",
                reference: null,
                digest: null,
                reason: null,
                deleted_at_unix: 1_767_225_600,
                deadline_unix: 1_767_312_000,
                state: "other",
                restorable: false,
              },
            ],
        next_cursor: cursor ? null : "next-trash",
      },
    });
  });
  await goto(page, "/");
  await page.locator('a[href="/admin/trash"]').click();
  await page.locator("#trash-user").fill("administrator");
  await page.locator("#trash-password").fill("browser-admin-secret");
  await page.locator("#trash-repository").fill(" hosted ");
  await page.locator("#trash-ecosystem").fill(" fixture ");
  await page.locator("#trash-state").selectOption("restorable");
  await page.locator("#trash-limit").selectOption("50");
  await page.getByRole("button", { name: "Search" }).click();
  await expect(page.getByRole("status")).toHaveText("Loaded 2 trash records.");
  await expect(page.locator(".trash-table tbody td")).toHaveText([
    "Restorable",
    "fixture",
    "hosted",
    "artifact.bin",
    "1.0",
    "sha256:fixture",
    "replaced",
    "User",
    "2026-01-01T00:00:00Z",
    "2026-01-02T00:00:00Z",
    "Other",
    "fixture",
    "hosted",
    "unknown.bin",
    "-",
    "-",
    "-",
    "-",
    "2026-01-01T00:00:00Z",
    "2026-01-02T00:00:00Z",
  ]);
  await page.getByRole("button", { name: "Next" }).click();
  await expect(
    page.getByText("No trash records matched these filters."),
  ).toBeVisible();
  await page.getByRole("button", { name: "Next" }).dispatchEvent("click");
  expect(cursors).toEqual([null, "next-trash"]);
  await page.getByRole("button", { name: "Previous" }).click();
  await expect(page.getByText("artifact.bin")).toBeVisible();
  expect(cursors).toEqual([null, "next-trash", null]);
});

boundaryCases(test, {
  title: "trash",
  cases: [
    {
      label: "invalid input",
      response: { status: 400 },
      expectedAlert: "One or more trash filters are invalid.",
    },
    {
      label: "invalid credentials",
      response: { status: 401 },
      expectedAlert: "The username or password was not accepted.",
    },
    {
      label: "denied access",
      response: { status: 403 },
      expectedAlert: "This token cannot inspect trash.",
    },
    {
      label: "missing repository",
      response: { status: 404 },
      expectedAlert:
        "The repository was not found or is not available to this user.",
    },
    {
      label: "a server failure",
      response: { status: 500 },
      expectedAlert: "The trash inspection service is unavailable.",
    },
    {
      label: "a malformed success",
      response: { status: 200, body: "{" },
      expectedAlert: "The trash inspection service returned invalid data.",
    },
    {
      label: "a network failure",
      expectedAlert: "The trash inspection service could not be reached.",
    },
  ],
  setupRoute: (page, boundary) =>
    page.route("**/+trash?*", (route) =>
      boundary.response ? route.fulfill(boundary.response) : route.abort(),
    ),
  navigate: async (page) => {
    await goto(page, "/");
    await page.locator('a[href="/admin/trash"]').click();
  },
  action: async (page) => {
    await page.locator("#trash-user").fill("User");
    await page.locator("#trash-password").fill("password");
    await page.getByRole("button", { name: "Search" }).click();
  },
});

test("login renders providers returned to the hydrated router", async ({
  page,
}) => {
  await page.route("**/_/session", (route) =>
    route.fulfill({ json: { user: null, providers: ["work", "personal"] } }),
  );
  await goto(page, "/");
  await page.locator('a[href="/login"]').click();
  await expect(
    page.getByRole("link", { name: "Sign in with work" }),
  ).toHaveAttribute("href", "/_/login/work");
  await expect(
    page.getByRole("link", { name: "Sign in with personal" }),
  ).toBeVisible();
});

test("login renders the current session", async ({ page }) => {
  await page.route("**/_/session", (route) =>
    route.fulfill({
      json: { user: { name: "Browser User" }, providers: ["work"] },
    }),
  );
  await goto(page, "/");
  await page.locator('a[href="/login"]').click();
  await expect(page.getByText("Signed in as")).toContainText("Browser User");
  await expect(page.getByRole("button", { name: "Log out" })).toBeVisible();
});

test("login falls back when session data is unavailable", async ({ page }) => {
  await page.route("**/_/session", (route) =>
    route.fulfill({ status: 200, body: "{" }),
  );
  await goto(page, "/");
  await page.locator('a[href="/login"]').click();
  await expect(
    page.getByText("No login providers are configured."),
  ).toBeVisible();
});
