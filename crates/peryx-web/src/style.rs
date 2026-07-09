//! The UI stylesheet, inlined into the page shell.
//!
//! Mirrors the documentation site's design tokens (brand gradient, light/dark palettes,
//! terminal-style code) so the served UI and the docs read as one product.

pub const CSS: &str = r"
:root {
  --bg: #f7f4ef; --bg-soft: #fffdf9; --bg-sink: #efeae1; --text: #1a1a1a; --text-soft: #3f3d3b;
  --text-faint: #6b6862; --accent: #c53d00; --accent-strong: #b23800; --brand-a: #f74c00; --brand-b: #ffb600;
  --border: #e6dfd2; --border-strong: #d8cfbe; --code-bg: #efeae1;
  --terminal-bg: #17140f; --terminal-text: #e7ddcf; --terminal-dim: #8a8175;
  --ok: #0ca30c; --warn: #d98a00; --bad: #d03b3b;
  color-scheme: light;
}
:root[data-theme='dark'] { color-scheme: dark; }
@media (prefers-color-scheme: dark) { :root:not([data-theme='light']) { color-scheme: dark; } }
@media (prefers-color-scheme: dark) {
  :root:not([data-theme='light']) {
    --bg: #131110; --bg-soft: #1b1815; --bg-sink: #100e0c; --text: #e7e7e9; --text-soft: #bcbcbe;
    --text-faint: #6f665a; --accent: #ff8a3d; --accent-strong: #ffb600;
    --border: #2c2822; --border-strong: #3a352d; --code-bg: #1c1915;
  }
}
:root[data-theme='dark'] {
  --bg: #131110; --bg-soft: #1b1815; --bg-sink: #100e0c; --text: #e7e7e9; --text-soft: #bcbcbe;
  --text-faint: #6f665a; --accent: #ff8a3d; --accent-strong: #ffb600;
  --border: #2c2822; --border-strong: #3a352d; --code-bg: #1c1915;
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
.error { color: var(--bad); font-family: ui-monospace, Menlo, monospace; font-size: 0.9rem; }
.ops-title { display: flex; align-items: center; gap: 0.6rem; flex-wrap: wrap; margin-bottom: 1rem; }
.ops-title h1 { margin: 0 0.4rem 0 0; }
.ops-title a code { color: inherit; }
.table-scroll { overflow-x: auto; }
.ops-table { margin-top: 0.8rem; }
/* The admin status page is data-dense (wide topology and usage tables), so it breaks out of the
   70rem reading column to a wider, viewport-centered width. The tables fit without scrolling on a
   desktop, and still scroll gracefully within `.table-scroll` on narrow screens. */
.ops-page { width: min(94rem, calc(100vw - 3rem)); margin-left: 50%; transform: translateX(-50%); }
.table-scroll .ops-table { min-width: 48rem; }
.ops-table th, .ops-table td { padding: 0.4rem 0.55rem; font-size: 0.85rem; }
.ops-table th { white-space: nowrap; }
.ops-table td { vertical-align: top; }
.ops-table .badge { font-size: 0.78rem; padding: 0.05rem 0.4rem; }
.ops-type { display: flex; gap: 0.3rem; flex-wrap: wrap; align-items: center; }
.ops-simple { white-space: nowrap; }
.ops-stack { list-style: none; margin: 0; padding: 0; }
.ops-stack li { display: flex; align-items: center; gap: 0.4rem; min-height: 1.6rem; }
.ops-stack li + li { margin-top: 0.2rem; }
.ops-detail { display: flex; gap: 0.45rem; flex-wrap: wrap; margin: 0; color: var(--text-soft); }
.badge.upload-enabled { color: var(--ok); border-color: var(--ok); }
.badge.upload-disabled { color: var(--text-soft); border-color: var(--border); }
.badge.status-configured { color: var(--ok); border-color: var(--ok); }
.metrics-group { margin: 0.75rem 0; }
.metrics-label {
  display: flex; align-items: center; gap: 0.4rem; margin-bottom: 0.5rem;
  font-size: 0.8rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em;
  color: var(--text-soft);
}
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
.badge.kind-cached { color: #2f81f7; border-color: #2f81f7; }
/* Ecosystem device: a neutral chip carrying the ecosystem's own colour as a leading dot, never colour
   alone. One `--eco` per ecosystem drives the dot, so adding a format is one line. */
.badge.ecosystem-pypi { --eco: #3775a9; }
.badge.ecosystem-oci { --eco: #2496ed; }
.badge[class*='ecosystem-'] { color: var(--text-soft); border-color: var(--border); }
.badge[class*='ecosystem-']::before {
  content: ''; display: inline-block; width: 0.5rem; height: 0.5rem; border-radius: 2px;
  background: var(--eco); margin-right: 0.4rem; vertical-align: -0.02em;
}
.badge.kind-hosted { color: var(--ok); border-color: var(--ok); }
.badge.kind-virtual { color: var(--accent); border-color: var(--accent); }
.badge.source-uploaded { color: var(--ok); border-color: var(--ok); }
.badge.source-cached { color: #2f81f7; border-color: #2f81f7; }
.badge.source-override { color: #8b5cf6; border-color: #8b5cf6; }
.badge.uploads { background: linear-gradient(115deg, var(--brand-a), var(--brand-b)); color: #fff; border: none; }
.badge.yanked-badge { color: var(--bad); border-color: var(--bad); }
.badge.meta-badge { color: var(--ok); border-color: var(--ok); }
.layers code { margin-right: 0.3rem; }
.virtual-card { grid-column: span 2; }
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
.admin button.danger:hover { border-color: var(--bad); color: var(--bad); }
.outcome { font-family: ui-monospace, Menlo, monospace; font-size: 0.85rem; color: var(--text-soft); }

/* The stoop: the home mark folds in from up and back, sheds speed streaks, and settles once on load.
   transform-box keeps the percentage origin on the falcon's own box so it does not drift. */
.hero-brand { display: flex; align-items: center; gap: 1.1rem; margin: 0 0 1.75rem; }
.hero-brand .stoop-stage { position: relative; width: 4.5rem; height: 4.5rem; flex: none; display: grid; place-items: center; }
.hero-brand .stoop { width: 4.5rem; height: 4.5rem; display: block; }
.hero-brand .stoop .falcon { transform-box: fill-box; transform-origin: 50% 60%; animation: stoop-dive 0.7s both; }
.hero-brand .streaks { position: absolute; inset: 0; pointer-events: none; }
.hero-brand .streaks span {
  position: absolute; top: 6%; width: 2px; border-radius: 2px; opacity: 0;
  background: linear-gradient(var(--brand-b), color-mix(in srgb, var(--brand-b) 0%, transparent));
  animation: stoop-streak 0.6s both;
}
.hero-brand .streaks span:nth-child(1) { left: 38%; height: 34%; }
.hero-brand .streaks span:nth-child(2) { left: 52%; height: 46%; animation-delay: 0.03s; }
.hero-brand .streaks span:nth-child(3) { left: 64%; height: 36%; animation-delay: 0.05s; }
.hero-brand .brand-text { display: flex; flex-direction: column; }
.hero-brand .wordmark {
  font-weight: 800; letter-spacing: -0.02em; font-size: 2rem; line-height: 1;
  background: linear-gradient(115deg, var(--brand-a), var(--brand-b));
  -webkit-background-clip: text; background-clip: text; color: transparent;
}
.hero-brand .tagline { color: var(--text-soft); font-size: 0.92rem; margin: 0.2rem 0 0; }
@keyframes stoop-dive {
  0% { opacity: 0; transform: translate3d(-22%, -64%, 0) rotate(-18deg) scale(0.5); animation-timing-function: cubic-bezier(0.5, 0, 0.82, 0.22); }
  40% { opacity: 1; }
  58% { transform: translate3d(3%, 9%, 0) rotate(3deg) scale(1.08); animation-timing-function: cubic-bezier(0.2, 0.9, 0.3, 1); }
  74% { transform: translate3d(0, -2%, 0) rotate(-1deg) scale(0.98); }
  100% { opacity: 1; transform: none; }
}
@keyframes stoop-streak {
  0% { opacity: 0; transform: translateY(-10px) scaleY(0.3); }
  38% { opacity: 0.9; }
  62% { opacity: 0.5; transform: translateY(8px) scaleY(1.4); }
  100% { opacity: 0; transform: translateY(20px) scaleY(0.5); }
}
/* Loading state: the same stoop, looped. */
.stoop-loader { display: flex; flex-direction: column; align-items: center; gap: 0.7rem; padding: 3.5rem 0; color: var(--text-soft); }
.stoop-loader .stoop { width: 3rem; height: 3rem; display: block; }
.stoop-loader .stoop .falcon { transform-box: fill-box; transform-origin: 50% 50%; animation: stoop-loop 1.15s linear infinite; }
.stoop-loader .cap { font-family: ui-monospace, Menlo, monospace; font-size: 0.72rem; letter-spacing: 0.08em; text-transform: uppercase; }
@keyframes stoop-loop {
  0% { opacity: 0; transform: translateY(-150%) scale(0.7); }
  18% { opacity: 1; }
  52% { transform: translateY(0) scale(1); opacity: 1; }
  82% { opacity: 1; }
  100% { opacity: 0; transform: translateY(150%) scale(0.9); }
}
@media (prefers-reduced-motion: reduce) {
  .hero-brand .stoop .falcon, .stoop-loader .stoop .falcon { animation: none; opacity: 1; transform: none; }
  .hero-brand .streaks { display: none; }
}
";
