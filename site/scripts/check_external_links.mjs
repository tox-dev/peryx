import { readFile, readdir } from "node:fs/promises";
import { extname, join } from "node:path";

const CONCURRENCY = 4;
const ATTEMPTS = 3;
const EXCLUSIONS = new Map([
  ["http://127.0.0.1:4433/", "examples target the reader's local peryx process"],
]);

const root = process.argv[2];
if (!root) throw new Error("usage: check_external_links.mjs <site-root>");
const baseUrl = baseUrlFrom(await readFile(join(root, "config.toml"), "utf8"));
const links = await linksFrom(join(root, "public"));
const excluded = [...links].filter(([url]) => exclusion(url));
const checked = [...links].filter(([url]) => !exclusion(url)).map(([url]) => url);
const local = checked.filter((url) => new URL(url).origin === baseUrl.origin);
const external = checked.filter((url) => new URL(url).origin !== baseUrl.origin);
const results = [...(await checkLocalLinks(root, local)), ...(await checkExternalLinks(external))];
for (const [url, occurrences] of excluded) {
  console.log(`excluded ${occurrences} occurrence(s): ${url} (${exclusion(url)})`);
}
const rateLimited = Map.groupBy(
  results.filter(({ state }) => state === "rate-limited"),
  ({ url }) => new URL(url).host,
);
for (const [host, hostResults] of rateLimited) {
  console.warn(`rate limited after ${ATTEMPTS} attempts: ${host} (${hostResults.length} link(s) not checked)`);
}
const failures = results.filter(({ state }) => state === "failed");
for (const { url, detail } of failures) console.error(`${url}: ${detail}`);
const rateLimitedCount = [...rateLimited.values()].flat().length;
console.log(
  `checked ${local.length} local and ${external.length} external link(s); ` +
    `${excluded.reduce((total, [, occurrences]) => total + occurrences, 0)} excluded occurrence(s); ` +
    `${results.length - failures.length - rateLimitedCount} passed; ${rateLimitedCount} rate limited`,
);
if (failures.length > 0) process.exitCode = 1;

function baseUrlFrom(config) {
  const value = config.match(/^base_url\s*=\s*"([^"]+)"/m)?.[1];
  if (!value) throw new Error("site config has no base_url");
  return new URL(value);
}

async function linksFrom(dir) {
  const links = new Map();
  for (const path of await files(dir)) {
    const content = await readFile(path, "utf8");
    for (const match of content.matchAll(/\b(?:href|src)=["'](https?:\/\/[^"']+)["']/g)) {
      const url = match[1].replaceAll("&amp;", "&");
      links.set(url, (links.get(url) ?? 0) + 1);
    }
  }
  return links;
}

async function files(dir) {
  return (
    await Promise.all(
      (await readdir(dir, { withFileTypes: true })).map(async (entry) => {
        const path = join(dir, entry.name);
        if (entry.isDirectory()) return files(path);
        return extname(entry.name) === ".html" ? [path] : [];
      }),
    )
  ).flat();
}

function exclusion(url) {
  return [...EXCLUSIONS].find(([prefix]) => url.startsWith(prefix))?.[1];
}

async function checkLocalLinks(root, urls) {
  return Promise.all(
    urls.map(async (url) => {
      const parsed = new URL(url);
      const path = join(
        root,
        "public",
        decodeURIComponent(parsed.pathname),
        parsed.pathname.endsWith("/") ? "index.html" : "",
      );
      try {
        const content = await readFile(path);
        const fragment = parsed.hash.slice(1);
        if (fragment && !hasAnchor(content.toString(), fragment)) {
          return { url, state: "failed", detail: `missing local anchor #${fragment}` };
        }
        return { url, state: "passed" };
      } catch (error) {
        return { url, state: "failed", detail: error.code === "ENOENT" ? "missing local target" : error.message };
      }
    }),
  );
}

async function checkExternalLinks(urls) {
  const hosts = Map.groupBy(urls, (url) => new URL(url).host);
  const queue = [...hosts.values()];
  const results = [];
  await Promise.all(
    Array.from({ length: Math.min(CONCURRENCY, queue.length) }, async () => {
      let hostUrls;
      while ((hostUrls = queue.shift())) results.push(...(await checkHost(hostUrls)));
    }),
  );
  return results;
}

async function checkHost(urls) {
  const results = [];
  for (const url of urls) {
    const result = await checkLink(url);
    results.push(result);
    if (result.state === "rate-limited") {
      results.push(...urls.slice(results.length).map((url) => ({ url, state: "rate-limited" })));
      break;
    }
  }
  return results;
}

async function checkLink(url) {
  for (let attempt = 1; attempt <= ATTEMPTS; attempt += 1) {
    try {
      const response = await fetch(new URL(url).origin + new URL(url).pathname + new URL(url).search, {
        headers: { "user-agent": "peryx-link-checker/1" },
        redirect: "follow",
        signal: AbortSignal.timeout(15_000),
      });
      if (response.status === 429) {
        await response.body?.cancel();
        if (attempt === ATTEMPTS) return { url, state: "rate-limited" };
        await wait(retryDelay(response, attempt));
        continue;
      }
      if (response.status >= 500 && attempt < ATTEMPTS) {
        await response.body?.cancel();
        await wait(attempt * 500);
        continue;
      }
      if (!response.ok) {
        await response.body?.cancel();
        return { url, state: "failed", detail: `HTTP ${response.status}` };
      }
      const fragment = new URL(url).hash.slice(1);
      if (fragment) {
        if (!(await responseHasAnchor(response, fragment))) {
          return { url, state: "failed", detail: `missing anchor #${fragment}` };
        }
      } else {
        await response.body?.cancel();
      }
      return { url, state: "passed" };
    } catch (error) {
      if (attempt === ATTEMPTS) return { url, state: "failed", detail: error.message };
      await wait(attempt * 500);
    }
  }
}

function retryDelay(response, attempt) {
  const header = Number(response.headers.get("retry-after"));
  return Number.isFinite(header) && header > 0 ? Math.min(header * 1_000, 5_000) : attempt * 500;
}

async function responseHasAnchor(response, fragment) {
  if (!response.headers.get("content-type")?.includes("text/html")) return false;
  return hasAnchor(await response.text(), fragment);
}

function hasAnchor(html, fragment) {
  const anchor = decodeURIComponent(fragment);
  return [`id="${anchor}"`, `id="user-content-${anchor}"`, `name="${anchor}"`].some((value) => html.includes(value));
}

function wait(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}
