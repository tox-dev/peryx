import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const siteDirectory = join(scriptDirectory, "..");
const lightConfig = join(scriptDirectory, "mermaid-light.json");
const darkConfig = join(scriptDirectory, "mermaid-dark.json");
const puppeteerConfig = join(scriptDirectory, "mermaid-puppeteer.json");
const check = process.argv.includes("--check");
const force = process.argv.includes("--force");
const mmdc = process.env.PERYX_MMDC || (existsSync(join(siteDirectory, "node_modules", ".bin", "mmdc"))
  ? join(siteDirectory, "node_modules", ".bin", "mmdc")
  : "mmdc");
const hostedLinux = process.platform === "linux" && (process.env.CI || process.env.READTHEDOCS);

const BLOCK = /\{%\s*(?:<mermaid>|mermaid\(\))\s*%\}\s*([\s\S]*?)\s*\{%\s*(?:<\/mermaid>|end)\s*%\}/g;

// Theme-specific fills keep labels legible on both page surfaces.
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
const QUOTED = /"(?:\\.|[^"\\])*"/gs;

// Mermaid rejects unused class definitions.
function withRoles(source, theme) {
  const used = new Set(Array.from(source.matchAll(ROLE_USE), (match) => match[1]));
  const defs = [...used].filter((role) => ROLES[theme][role]).map((role) => `classDef ${role} ${ROLES[theme][role]}`);
  return defs.length ? `${source}\n${defs.join("\n")}` : source;
}

function normalizeSource(source) {
  const normalized = source.replace(/\\([\[\]])/g, "$1").replace(QUOTED, (quoted) => quoted.replace(/\s*\n\s*/g, " "));
  if (/^sequenceDiagram[^\S\r\n]+\S/.test(normalized)) {
    return normalized
      .replace(/\s+/g, " ")
      .replace(/^sequenceDiagram\s+/, "sequenceDiagram\n")
      .replace(/\s+(?=participant\s)/g, "\n")
      .replace(/\s+(?=[A-Za-z_][\w-]*\s*(?:->>|-->>))/g, "\n");
  }
  if (/^stateDiagram-v2[^\S\r\n]+\S/.test(normalized)) {
    return normalized
      .replace(/\s+/g, " ")
      .replace(/^stateDiagram-v2\s+/, "stateDiagram-v2\n")
      .replace(/\s+(?=(?:\[\*\]|[A-Za-z_][\w-]*)\s+-->)/g, "\n");
  }
  if (!/^(flowchart|graph)\s/.test(normalized)) return normalized;
  if ((normalized.replace(QUOTED, "").match(/;/g) || []).length > 1) {
    return normalized.replace(/\s*\n\s*/g, " ").replace(/^((?:flowchart|graph)\s+\w+)\s*;?\s*/, "$1;\n");
  }
  if (!/^(?:flowchart|graph)\s+\w+[^\S\r\n]+\S/.test(normalized)) return normalized;
  return normalized
    .replace(/\s+/g, " ")
    .replace(/^((?:flowchart|graph)\s+\w+)\s+/, "$1\n")
    .replace(/([}\]])\s+(?=[A-Za-z_][\w-]*\s+(?:-->|---|-.->|==>))/g, "$1\n")
    .replace(/\s+(?=class\s)/g, "\n");
}

function markdownFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).sort((left, right) => left.name.localeCompare(right.name)).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return markdownFiles(path);
    return entry.name.endsWith(".md") ? [path] : [];
  });
}

function contentOwners() {
  const owners = [{
    content: join(siteDirectory, "content"),
    diagrams: join(siteDirectory, "diagrams"),
    name: "core",
  }];
  const cratesDirectory = join(siteDirectory, "..", "crates");
  if (!existsSync(cratesDirectory)) return owners;
  for (const entry of readdirSync(cratesDirectory, { withFileTypes: true }).sort((left, right) => left.name.localeCompare(right.name))) {
    if (!entry.isDirectory()) continue;
    const docsDirectory = join(cratesDirectory, entry.name, "docs");
    const content = join(docsDirectory, "content");
    if (existsSync(join(docsDirectory, "ecosystem.toml")) && existsSync(content)) {
      owners.push({ content, diagrams: join(docsDirectory, "diagrams"), name: entry.name });
    }
  }
  return owners;
}

function svgBody(path) {
  // XML prologs cannot be nested in an HTML document.
  const text = readFileSync(path, "utf8");
  return text.slice(text.indexOf("<svg"));
}

const VIEWBOX = /viewBox="[-\d.]+ [-\d.]+ ([\d.]+) ([\d.]+)"/;

// Intrinsic dimensions prevent browser fallback sizing and a white dark-theme background.
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
  // Unique IDs prevent hidden theme variants from repainting visible diagrams.
  const variant = (config, name) => {
    const out = join(tmp, `${name}.svg`);
    const id = `peryx-${hash}-${name}`;
    writeFileSync(input, withRoles(source, name));
    // A stuck browser must fail the docs lane instead of occupying it indefinitely.
    const puppeteerArgs = hostedLinux ? ["--puppeteerConfigFile", puppeteerConfig] : [];
    execFileSync(mmdc, [
      "--input", input,
      "--output", out,
      "--configFile", config,
      ...puppeteerArgs,
      "--svgId", id,
      "--quiet",
    ], {
      stdio: ["ignore", "ignore", "inherit"],
      timeout: 60_000,
      killSignal: "SIGKILL",
    });
    return normalizeRoot(svgBody(out));
  };
  return { light: variant(lightConfig, "light"), dark: variant(darkConfig, "dark") };
}

function validatePartial(path, hash) {
  const partial = readFileSync(path, "utf8");
  for (const marker of [
    '<figure class="mermaid-figure">',
    '<div class="mermaid-svg mermaid-light">',
    '<div class="mermaid-svg mermaid-dark">',
    `id="peryx-${hash}-light"`,
    `id="peryx-${hash}-dark"`,
  ]) {
    if (!partial.includes(marker)) throw new Error(`${path} is missing ${marker}`);
  }
}

function main() {
  const owners = contentOwners();
  for (const owner of owners) mkdirSync(owner.diagrams, { recursive: true });
  const tmp = check ? null : mkdtempSync(join(tmpdir(), "peryx-diagrams-"));
  const sources = new Map();
  const kept = new Map(owners.map((owner) => [owner.diagrams, new Set()]));
  let count = 0;
  try {
    for (const owner of owners) {
      for (const file of markdownFiles(owner.content)) {
        const text = readFileSync(file, "utf8");
        let block = 0;
        for (const [, raw] of text.matchAll(BLOCK)) {
          block += 1;
          const source = raw.trim();
          const hash = createHash("sha256").update(source).digest("hex").slice(0, 16);
          const location = `${file}#${block}`;
          if (sources.has(hash)) throw new Error(`duplicate diagram ${hash}: ${sources.get(hash)} and ${location}`);
          sources.set(hash, location);
          const name = `${hash}.html`;
          const output = join(owner.diagrams, name);
          kept.get(owner.diagrams).add(name);
          if (existsSync(output) && !force) {
            validatePartial(output, hash);
            count += 1;
            continue;
          }
          if (check) throw new Error(`${file} requires ${output}`);
          const { light: lightSvg, dark: darkSvg } = render(normalizeSource(source), hash, tmp);
          const partial =
            `<figure class="mermaid-figure">` +
            `<div class="mermaid-svg mermaid-light">${lightSvg}</div>` +
            `<div class="mermaid-svg mermaid-dark">${darkSvg}</div>` +
            `</figure>\n`;
          writeFileSync(output, partial);
          count += 1;
        }
      }
    }
    for (const owner of owners) {
      for (const name of readdirSync(owner.diagrams)) {
        if (kept.get(owner.diagrams).has(name)) continue;
        const orphan = join(owner.diagrams, name);
        if (check) throw new Error(`${orphan} has no source`);
        rmSync(orphan, { recursive: true, force: true });
      }
    }
  } finally {
    if (tmp !== null) rmSync(tmp, { recursive: true, force: true });
  }
  console.log(`${check ? "checked" : "rendered"} ${count} diagram(s) across ${owners.length} owner(s)`);
}

main();
