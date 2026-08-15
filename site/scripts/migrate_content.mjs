import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";

const INLINE = /\{\{\s*([A-Za-z_][\w.-]*)\((.*?)\)\s*\}\}/gs;
const PAIRED = /\{%\s*([A-Za-z_][\w.-]*)\((.*?)\)\s*%\}([\s\S]*?)\{%\s*end\s*%\}/g;
const FENCED = /^(`{3,}|~{3,})[^\n]*\n[\s\S]*?^\1[ \t]*$/gm;
const RELATIVE_MARKDOWN_LINK = /(\]\()((?:\.{0,2}\/)*[A-Za-z0-9_./-]+\.md(?:#[^)\s]+)?)(\))/g;

function main() {
  if (process.argv.length < 3 || process.argv.length > 4)
    throw new Error("usage: migrate_content.mjs <content-dir> [ecosystem-owner-links]");
  const root = process.argv[2];
  const ownerLinks = process.argv[3] ? readFileSync(process.argv[3], "utf8").trimEnd() : undefined;
  for (const path of markdownFiles(root)) {
    const source = readFileSync(path, "utf8");
    const fences = [];
    let migrated = (ownerLinks ? source.replaceAll("{{ ecosystem_owner_links() }}", ownerLinks) : source)
      .replace(FENCED, (block) => `PERYX_FENCED_BLOCK_${fences.push(block) - 1}`)
      .replace(PAIRED, (_, name, args, body) => `{% <${name}${componentArgs(args)}> %}${body}{% </${name}> %}`)
      .replace(INLINE, (_, name, args) => `{{<${name}${componentArgs(args)} />}}`);
    const owner = ownerRoot(root, path);
    if (owner) {
      migrated = migrated.replace(/@\/([A-Za-z0-9_-]+)/g, (link, prefix) =>
        owner.prefixes.has(prefix) ? `@/ecosystems/${owner.name}/${prefix}` : link,
      );
      migrated = migrateOwnerLinks(migrated, path, owner);
    }
    migrated = migrated.replace(/PERYX_FENCED_BLOCK_(\d+)/g, (_, index) => escapeTemplateSyntax(fences[index]));
    if (migrated !== source) writeFileSync(path, migrated);
  }
}

function ownerRoot(root, path) {
  const [ecosystems, name, ...ownerPath] = relative(root, path).split(sep);
  if (ecosystems !== "ecosystems" || !name || ownerPath.length === 0) return undefined;
  const dir = join(root, ecosystems, name);
  return {
    dir,
    name,
    prefixes: new Set(
      readdirSync(dir, { withFileTypes: true })
        .map((entry) => (entry.isDirectory() ? entry.name : entry.name.replace(/\.md$/, "")))
        .filter((entry) => entry !== "_index"),
    ),
  };
}

function migrateOwnerLinks(content, path, owner) {
  return content.replace(RELATIVE_MARKDOWN_LINK, (_, open, target, close) => {
    const [file, fragment] = target.split("#", 2);
    const destination = relative(owner.dir, resolve(dirname(path), file)).split(sep).join("/");
    if (destination.startsWith("../")) throw new Error(`${path}: owner link escapes its content root: ${target}`);
    return `${open}@/ecosystems/${owner.name}/${destination}${fragment ? `#${fragment}` : ""}${close}`;
  });
}

function markdownFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return markdownFiles(path);
    return entry.name.endsWith(".md") ? [path] : [];
  });
}

function componentArgs(args) {
  const migrated = args
    .trim()
    .replace(/,\s+(?=[A-Za-z_][\w-]*\s*=)/g, " ")
    .replace(/=(true|false|\d+)(?=\s|$)/g, "={$1}");
  return migrated ? ` ${migrated}` : "";
}

function escapeTemplateSyntax(block) {
  return block.replaceAll("{{", "&#123;&#123;").replaceAll("{%", "&#123;%");
}

main();
