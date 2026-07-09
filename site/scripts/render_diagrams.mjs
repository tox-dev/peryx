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

function render(source, tmp) {
  const input = join(tmp, "diagram.mmd");
  writeFileSync(input, source);
  const variant = (config) => {
    const out = join(tmp, "out.svg");
    execFileSync(mmdc, ["--input", input, "--output", out, "--configFile", config, "--quiet"], {
      stdio: ["ignore", "ignore", "inherit"],
    });
    return svgBody(out);
  };
  return { light: variant(light), dark: variant(dark) };
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
      const { light: l, dark: d } = render(source, tmp);
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
