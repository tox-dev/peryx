// Pre-render every mermaid diagram in the content tree to a committed dual-theme SVG partial.
//
// The docs used to ship mermaid.js from a CDN and render in the browser, which cost a cold
// multi-chunk fetch and a client-side render on first load of every diagram page. Instead we render
// each diagram to static SVG once, here, in both the light and dark palettes; `inline_diagrams.py`
// injects the partial into the built HTML, so the site ships zero diagram JavaScript.
//
// Diagrams are keyed by a hash of their source, so `inline_diagrams.py` can match a `<pre
// class="mermaid">` block to its partial without threading an id through the shortcode. Run this
// whenever a diagram changes; CI regenerates and fails if the committed partials are stale.
//
// Usage: node site/scripts/render_diagrams.mjs

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const site = join(here, "..");
const contentDir = join(site, "content");
const outDir = join(site, "diagrams");
const light = join(here, "mermaid-light.json");
const dark = join(here, "mermaid-dark.json");
const mmdc = existsSync(join(site, "node_modules", ".bin", "mmdc"))
  ? join(site, "node_modules", ".bin", "mmdc")
  : "mmdc";

const BLOCK = /\{%\s*mermaid\(\)\s*%\}\n([\s\S]*?)\n\{%\s*end\s*%\}/g;

// A diagram names a role — `class cache accent` — and the palette for that role lives here, once per
// theme, rather than as a `classDef` line repeated in every diagram. One palette baked into the
// source cannot suit both pages: a fill that reads on cream glares on the dark page. So each role is
// a tinted chip of its hue on the page's own surface, with the hue itself only on the border and the
// text — legible without the saturated block fills mermaid reaches for by default.
const ROLES = {
  light: {
    accent: "fill:#dbe6f5,stroke:#4a6f9f,color:#16304d",
    good: "fill:#d8ebe1,stroke:#3f8467,color:#14402f",
    warn: "fill:#f6e2d0,stroke:#b06f36,color:#5a3212",
  },
  dark: {
    accent: "fill:#1e2b3a,stroke:#6d9fd6,color:#cfe0f5",
    good: "fill:#182d25,stroke:#5aa98a,color:#cfe8dc",
    warn: "fill:#33241a,stroke:#c9843f,color:#f0d6bb",
  },
};

const ROLE_USE = /^\s*class\s+[\w,]+\s+(\w+)\s*$/gm;

// mermaid rejects a classDef for a role the diagram never uses, and sequence diagrams take none at
// all, so only the roles a diagram actually names are appended.
function withRoles(source, theme) {
  const used = new Set(Array.from(source.matchAll(ROLE_USE), (match) => match[1]));
  const defs = [...used].filter((role) => ROLES[theme][role]).map((role) => `classDef ${role} ${ROLES[theme][role]}`);
  return defs.length ? `${source}\n${defs.join("\n")}` : source;
}

function markdownFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return markdownFiles(path);
    return entry.name.endsWith(".md") ? [path] : [];
  });
}

function svgBody(path) {
  // mmdc writes an XML prolog; keep only the <svg> element so it inlines cleanly.
  const text = readFileSync(path, "utf8");
  return text.slice(text.indexOf("<svg"));
}

const VIEWBOX = /viewBox="[-\d.]+ [-\d.]+ ([\d.]+) ([\d.]+)"/;

// mmdc sizes the root as `width="100%" style="max-width: Npx; background-color: white"`. A percentage
// width leaves the SVG with no intrinsic width inside the centering container, so it collapses to the
// 300px CSS default for replaced elements, and the baked-in white survives into the dark variant.
// Carry the viewBox dimensions on the element instead and let the stylesheet cap it at the column.
function normalizeRoot(svg) {
  const end = svg.indexOf(">") + 1;
  const [, width, height] = VIEWBOX.exec(svg.slice(0, end));
  const open = svg
    .slice(0, end)
    .replace(/\s(?:style|width|height)="[^"]*"/g, "")
    .replace("<svg", `<svg width="${width}" height="${height}"`);
  return open + svg.slice(end);
}

function render(source, hash, tmp) {
  const input = join(tmp, "diagram.mmd");
  // Both variants sit in the DOM at once, so they cannot share mermaid's default `my-svg` id: an
  // SVG `<style>` applies document-wide even under `display: none`, so the later block would repaint
  // the visible variant in the other palette, and every `url(#…)` marker reference would resolve to
  // whichever copy came first.
  const variant = (config, name) => {
    const out = join(tmp, `${name}.svg`);
    const id = `peryx-${hash}-${name}`;
    writeFileSync(input, withRoles(source, name));
    execFileSync(mmdc, ["--input", input, "--output", out, "--configFile", config, "--svgId", id, "--quiet"], {
      stdio: ["ignore", "ignore", "inherit"],
    });
    return normalizeRoot(svgBody(out));
  };
  return { light: variant(light, "light"), dark: variant(dark, "dark") };
}

function main() {
  mkdirSync(outDir, { recursive: true });
  const tmp = mkdtempSync(join(tmpdir(), "peryx-diagrams-"));
  const kept = new Set();
  let count = 0;
  for (const file of markdownFiles(contentDir)) {
    const text = readFileSync(file, "utf8");
    for (const [, raw] of text.matchAll(BLOCK)) {
      const source = raw.trim();
      const hash = createHash("sha256").update(source).digest("hex").slice(0, 16);
      kept.add(`${hash}.html`);
      const { light: l, dark: d } = render(source, hash, tmp);
      const partial =
        `<figure class="mermaid-figure">` +
        `<div class="mermaid-svg mermaid-light">${l}</div>` +
        `<div class="mermaid-svg mermaid-dark">${d}</div>` +
        `</figure>\n`;
      writeFileSync(join(outDir, `${hash}.html`), partial);
      count += 1;
    }
  }
  for (const name of readdirSync(outDir)) {
    if (name.endsWith(".html") && !kept.has(name)) rmSync(join(outDir, name));
  }
  rmSync(tmp, { recursive: true, force: true });
  console.log(`rendered ${count} diagram(s) to ${outDir}`);
}

main();
