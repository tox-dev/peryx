//! The UI stylesheet, inlined into the page shell.
//!
//! Mirrors the documentation site's design tokens (brand gradient, light/dark palettes,
//! terminal-style code) so the served UI and the docs read as one product.

pub const CSS: &str = r"
:root {
  --bg: #ffffff; --bg-soft: #f7f6f4; --text: #1c2026; --text-soft: #555d68;
  --accent: #d94400; --accent-strong: #b23800; --brand-a: #f74c00; --brand-b: #ffb600;
  --border: #e7e4df; --code-bg: #f3f1ed; --terminal-bg: #1e2226; --terminal-text: #e7ebf0;
  color-scheme: light;
}
:root[data-theme='dark'] { color-scheme: dark; }
@media (prefers-color-scheme: dark) { :root:not([data-theme='light']) { color-scheme: dark; } }
@media (prefers-color-scheme: dark) {
  :root:not([data-theme='light']) {
    --bg: #12151a; --bg-soft: #191d24; --text: #e8ebee; --text-soft: #9aa4b0;
    --accent: #ff8a3d; --accent-strong: #ffb600; --border: #2a2f38; --code-bg: #1c212a;
  }
}
:root[data-theme='dark'] {
  --bg: #12151a; --bg-soft: #191d24; --text: #e8ebee; --text-soft: #9aa4b0;
  --accent: #ff8a3d; --accent-strong: #ffb600; --border: #2a2f38; --code-bg: #1c212a;
}
* { box-sizing: border-box; }
body {
  margin: 0; font-size: 16px; line-height: 1.6; color: var(--text); background: var(--bg);
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', sans-serif;
}
a { color: var(--accent); text-decoration: none; }
a:hover { color: var(--accent-strong); text-decoration: underline; }
code {
  font-family: ui-monospace, 'SF Mono', Menlo, Consolas, monospace; font-size: 0.9em;
  background: var(--code-bg); border-radius: 5px; padding: 0.1em 0.35em;
}
.site-header {
  position: sticky; top: 0; z-index: 10; border-bottom: 1px solid var(--border);
  background: color-mix(in srgb, var(--bg) 85%, transparent); backdrop-filter: blur(10px);
}
.site-header nav {
  max-width: 70rem; margin: 0 auto; padding: 0.7rem 1.25rem;
  display: flex; align-items: center; justify-content: space-between; gap: 1rem;
}
.brand { display: flex; align-items: center; gap: 0.5rem; font-weight: 700; font-size: 1.15rem; color: var(--text); }
.brand:hover { text-decoration: none; }
.nav-links { display: flex; gap: 1rem; align-items: center; }
.nav-links a { color: var(--text-soft); font-size: 0.95rem; }
.nav-links a:hover { color: var(--accent); text-decoration: none; }
.header-search { position: relative; flex: 1 1 18rem; max-width: 24rem; }
.header-search input[type='search'] {
  width: 100%; height: 2.2rem; padding: 0 0.75rem; border: 1px solid var(--border);
  border-radius: 8px; background: var(--bg); color: var(--text); font-size: 0.9rem;
}
.header-search input[type='search']:focus { outline: 2px solid color-mix(in srgb, var(--brand-a) 45%, transparent); }
.suggestions {
  position: absolute; top: calc(100% + 0.35rem); left: 0; right: 0; z-index: 20;
  border: 1px solid var(--border); border-radius: 8px; background: var(--bg);
  box-shadow: 0 12px 30px color-mix(in srgb, var(--text) 12%, transparent); overflow: hidden;
}
.suggestion {
  display: grid; grid-template-columns: minmax(0, 1fr) auto auto; gap: 0.5rem; align-items: center;
  padding: 0.45rem 0.65rem; color: var(--text); font-size: 0.86rem;
}
.suggestion:hover { background: var(--bg-soft); text-decoration: none; }
.suggestion code { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.suggestion.all-results { display: block; border-top: 1px solid var(--border); color: var(--accent); font-weight: 600; }
.theme-toggle {
  border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: var(--text-soft);
  width: 2rem; height: 2rem; cursor: pointer; font-size: 0.95rem; line-height: 1;
}
.theme-toggle:hover { border-color: var(--accent); color: var(--accent); }
main { max-width: 70rem; margin: 0 auto; padding: 2rem 1.25rem 4rem; }
.page h1 { letter-spacing: -0.02em; margin-top: 0; }
.page h2 { margin-top: 2rem; border-bottom: 1px solid var(--border); padding-bottom: 0.3rem; }
.dim { color: var(--text-soft); }
.error { color: #e5484d; font-family: ui-monospace, Menlo, monospace; font-size: 0.9rem; }
.ops-title { display: flex; align-items: center; gap: 0.6rem; flex-wrap: wrap; margin-bottom: 1rem; }
.ops-title h1 { margin: 0 0.4rem 0 0; }
.ops-title a code { color: inherit; }
.table-scroll { overflow-x: auto; }
.ops-table { margin-top: 0.8rem; }
.table-scroll .ops-table { min-width: 72rem; }
.ops-table th { white-space: nowrap; }
.ops-table td { vertical-align: top; }
.ops-stack { list-style: none; margin: 0; padding: 0; }
.ops-stack li { display: flex; align-items: center; gap: 0.4rem; min-height: 1.6rem; }
.ops-stack li + li { margin-top: 0.2rem; }
.ops-detail { display: flex; gap: 0.45rem; flex-wrap: wrap; margin: 0; color: var(--text-soft); }
.badge.upload-enabled { color: #34c496; border-color: #34c496; }
.badge.upload-disabled { color: var(--text-soft); border-color: var(--border); }
.badge.status-configured { color: #34c496; border-color: #34c496; }
.stat-row { display: grid; grid-template-columns: repeat(auto-fit, minmax(11rem, 1fr)); gap: 1rem; }
.stat {
  border: 1px solid var(--border); border-radius: 12px; padding: 1rem 1.2rem; background: var(--bg-soft);
  text-align: center;
}
.stat strong { display: block; font-size: 1.4rem; letter-spacing: -0.01em; }
.stat span { color: var(--text-soft); font-size: 0.85rem; }
.index-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(18rem, 1fr)); gap: 1rem; }
.card {
  border: 1px solid var(--border); border-radius: 12px; padding: 1rem 1.2rem; background: var(--bg);
  transition: border-color 120ms ease, transform 120ms ease;
}
.card:hover { border-color: color-mix(in srgb, var(--brand-a) 55%, var(--border)); transform: translateY(-2px); }
.card-head { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
.card-title { font-weight: 700; font-size: 1.1rem; }
.badge {
  border-radius: 999px; padding: 0.1rem 0.6rem; font-size: 0.75rem; font-weight: 600;
  border: 1px solid var(--border); color: var(--text-soft);
}
.badge.kind-mirror { color: #2f81f7; border-color: #2f81f7; }
.badge.kind-local { color: #34c496; border-color: #34c496; }
.badge.kind-overlay { color: var(--accent); border-color: var(--accent); }
.badge.source-hosted { color: #34c496; border-color: #34c496; }
.badge.source-upstream { color: #2f81f7; border-color: #2f81f7; }
.badge.source-upstream-overrides { color: #8b5cf6; border-color: #8b5cf6; }
.badge.uploads { background: linear-gradient(120deg, var(--brand-a), var(--brand-b)); color: #fff; border: none; }
.badge.yanked-badge { color: #e5484d; border-color: #e5484d; }
.badge.meta-badge { color: #34c496; border-color: #34c496; }
.layers code { margin-right: 0.3rem; }
.overlay-card { grid-column: span 2; }
.layer-stack {
  list-style: none;
  margin: 0.6rem 0 0.2rem;
  padding: 0;
}
.layer {
  display: flex;
  align-items: center;
  gap: 0.55rem;
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--bg);
  padding: 0.45rem 0.7rem;
}
.layer + .layer {
  margin-top: -1px;
  border-top-left-radius: 0;
  border-top-right-radius: 0;
  margin-left: 0.9rem;
  opacity: 0.92;
}
.layer:first-child:not(:only-child) {
  border-bottom-left-radius: 0;
  border-bottom-right-radius: 0;
  border-left: 3px solid var(--accent);
}
.layer-order {
  font-size: 0.72rem;
  font-weight: 700;
  color: var(--text-soft);
  border: 1px solid var(--border);
  border-radius: 50%;
  width: 1.25rem;
  height: 1.25rem;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
}
.layer-name { font-weight: 600; }
.layer-route {
  margin-left: auto;
  font-size: 0.78rem;
  color: var(--text-soft);
}
.layer-hint {
  font-size: 0.78rem;
  color: var(--text-soft);
  margin: 0.35rem 0 0;
}
.card-usage { display: flex; gap: 0.8rem; font-size: 0.85rem; color: var(--text-soft); margin-top: 0.5rem; }
.card-usage a { margin-left: auto; }
.stats-table td { font-variant-numeric: tabular-nums; }
.search, .token {
  width: 100%; max-width: 28rem; padding: 0.55rem 0.9rem; margin: 0.75rem 0 1rem;
  border: 1px solid var(--border); border-radius: 9px; background: var(--bg); color: var(--text);
  font-size: 0.95rem;
}
.search:focus, .token:focus { outline: 2px solid color-mix(in srgb, var(--brand-a) 45%, transparent); }
.search-controls {
  display: grid; grid-template-columns: minmax(16rem, 1fr) auto auto auto; gap: 0.65rem; align-items: center;
  margin: 0.8rem 0 1.2rem;
}
.search-controls .search { max-width: none; margin: 0; }
.search-controls select, .search-controls button {
  height: 2.45rem; border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: var(--text);
  padding: 0 0.65rem; font-size: 0.9rem;
}
.search-controls button { cursor: pointer; color: var(--accent); font-weight: 600; }
.search-controls button:hover { border-color: var(--accent); }
.result-count { color: var(--text-soft); margin: 0 0 0.6rem; }
.search-results { min-width: 58rem; }
.search-results td:last-child { color: var(--text-soft); min-width: 16rem; }
.pagination { display: flex; align-items: center; gap: 0.75rem; margin-top: 1rem; }
.page-link {
  border: 1px solid var(--border); border-radius: 7px; padding: 0.3rem 0.75rem; color: var(--accent);
}
.page-link:hover { border-color: var(--accent); text-decoration: none; }
.page-link.disabled { color: var(--text-soft); background: var(--bg-soft); }
.project-list { list-style: none; padding: 0; columns: 3 14rem; }
.project-list li { padding: 0.2rem 0; break-inside: avoid; }
.breadcrumb { color: var(--text-soft); font-size: 0.9rem; }
.project-head .version { color: var(--text-soft); font-weight: 400; font-size: 1.2rem; margin-left: 0.5rem; }
.summary { color: var(--text-soft); font-size: 1.05rem; margin-top: -0.4rem; }
.install {
  display: flex; align-items: center; gap: 0.6rem; background: var(--terminal-bg); color: var(--terminal-text);
  border-radius: 10px; padding: 0.7rem 1rem; margin: 1rem 0; overflow-x: auto;
}
.install code { background: none; color: inherit; padding: 0; }
.copy {
  margin-left: auto; border: 1px solid #3a4048; background: none; color: var(--brand-b);
  border-radius: 7px; padding: 0.25rem 0.7rem; cursor: pointer; font-size: 0.8rem;
}
.copy:hover { border-color: var(--brand-b); }
.project-grid { display: grid; grid-template-columns: 2fr 1fr; gap: 2.5rem; }
@media (max-width: 52rem) { .project-grid { grid-template-columns: 1fr; } }
@media (max-width: 52rem) {
  .site-header nav { flex-wrap: wrap; }
  .header-search { order: 3; flex-basis: 100%; max-width: none; }
  .nav-links { margin-left: auto; }
  .search-controls { grid-template-columns: 1fr 1fr; }
  .search-controls .search { grid-column: 1 / -1; }
}
.description :is(h1, h2, h3) { border: none; }
.description pre {
  background: var(--terminal-bg); color: var(--terminal-text); border-radius: 10px; padding: 1rem 1.2rem;
  overflow-x: auto;
}
.description pre code { background: none; color: inherit; padding: 0; }
.description img { max-width: 100%; }
.description-plain { white-space: pre-wrap; }
.file-filter { display: flex; align-items: center; gap: 0.7rem; flex-wrap: wrap; margin: 0 0 0.8rem; }
.file-search { flex: 1 1 18rem; margin: 0; }
.file-filter-mode { display: inline-flex; align-items: center; gap: 0.35rem; white-space: nowrap; }
.file-filter-count { color: var(--text-soft); font-size: 0.9rem; margin-left: auto; }
table.files, table.admin-table { border-collapse: collapse; width: 100%; font-size: 0.92rem; }
table.files th, table.files td, table.admin-table td {
  border: 1px solid var(--border); padding: 0.45rem 0.7rem; text-align: left;
}
table.files th { background: var(--bg-soft); }
table.files td.empty { color: var(--text-soft); text-align: center; }
tr.yanked td a { text-decoration: line-through; color: var(--text-soft); }
.project-side h3 { margin-bottom: 0.3rem; border-bottom: 1px solid var(--border); padding-bottom: 0.2rem; }
.chips code { margin: 0 0.3rem 0.3rem 0; display: inline-block; }
.classifiers { list-style: none; padding: 0; margin: 0 0 0.6rem; color: var(--text-soft); font-size: 0.85rem; }
.classifier-group { margin: 0.5rem 0 0.1rem; font-weight: 600; font-size: 0.85rem; }
.member-content {
  background: var(--terminal-bg); color: var(--terminal-text); border-radius: 10px; padding: 1rem 1.2rem;
  overflow-x: auto; font-size: 0.85rem;
}
.archive-tree, .archive-tree ul { list-style: none; margin: 0; padding-left: 1.1rem; }
.archive-tree { padding-left: 0; font-size: 0.92rem; }
.archive-tree li { min-height: 1.75rem; line-height: 1.75rem; }
.archive-tree summary { cursor: pointer; }
.archive-name { font-family: ui-monospace, Menlo, monospace; }
.archive-name.folder { color: var(--text); font-weight: 600; }
.archive-name.kind-archive { font-weight: 600; }
.archive-name.kind-binary, .archive-name.kind-unknown { color: var(--text-soft); }
.archive-meta { color: var(--text-soft); margin-left: 0.55rem; font-size: 0.82rem; }
.button-link {
  display: inline-block; border: 1px solid var(--border); border-radius: 7px; padding: 0.3rem 0.75rem;
  background: var(--bg); color: var(--accent);
}
.button-link:hover { border-color: var(--accent); text-decoration: none; }
.inspect { font-size: 0.85rem; }
.links-list { list-style: none; padding: 0; }
.admin { margin-top: 2rem; border: 1px solid var(--border); border-radius: 12px; padding: 0.8rem 1.2rem; }
.admin summary { cursor: pointer; font-weight: 600; }
.admin button {
  border: 1px solid var(--border); background: var(--bg); color: var(--text); border-radius: 7px;
  padding: 0.25rem 0.7rem; cursor: pointer; margin: 0.15rem 0.3rem 0.15rem 0; font-size: 0.85rem;
}
.admin button:hover { border-color: var(--accent); color: var(--accent); }
.admin button.danger:hover { border-color: #e5484d; color: #e5484d; }
.outcome { font-family: ui-monospace, Menlo, monospace; font-size: 0.85rem; color: var(--text-soft); }
";
