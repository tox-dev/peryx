//! The UI stylesheet, inlined into the page shell.
//!
//! Mirrors the documentation site's design tokens (brand gradient, light/dark palettes,
//! terminal-style code) so the served UI and the docs read as one product.

pub const CSS: &str = r"
:root {
  --bg: #f7f4ef; --bg-soft: #fffdf9; --bg-sink: #efeae1; --text: #1a1a1a; --heading: #111111; --text-strong: #111111; --text-soft: #3f3d3b;
  --text-faint: #6b6862; --accent: #c53d00; --accent-strong: #b23800; --brand-a: #f74c00; --brand-b: #ffb600;
  --gold-fg: #8a6200;
  --border: #e6dfd2; --border-strong: #d8cfbe; --code-bg: #efeae1;
  --terminal-bg: #17140f; --terminal-text: #e7ddcf; --terminal-dim: #8a8175;
  --ok: #0a7d0a; --warn: #8f5a00; --bad: #c62222;
  color-scheme: light;
}
:root[data-theme='dark'] { color-scheme: dark; }
@media (prefers-color-scheme: dark) { :root:not([data-theme='light']) { color-scheme: dark; } }
@media (prefers-color-scheme: dark) {
  :root:not([data-theme='light']) {
    --bg: #131110; --bg-soft: #1b1815; --bg-sink: #100e0c; --text: #e5e5e5; --heading: #fafafa; --text-strong: #f0f0f0; --text-soft: #bcbcbe;
    --text-faint: #8f867a; --accent: #d9682f; --accent-strong: #e07838; --gold-fg: #ffb600;
    --ok: #2f9d2f; --warn: #c48a2c; --bad: #df5b5b;
    --border: #2c2822; --border-strong: #3a352d; --code-bg: #1c1915;
  }
}
:root[data-theme='dark'] {
  --bg: #131110; --bg-soft: #1b1815; --bg-sink: #100e0c; --text: #e5e5e5; --heading: #fafafa; --text-strong: #f0f0f0; --text-soft: #bcbcbe;
  --text-faint: #8f867a; --accent: #d9682f; --accent-strong: #e07838; --gold-fg: #ffb600;
  --ok: #2f9d2f; --warn: #c48a2c; --bad: #df5b5b;
  --border: #2c2822; --border-strong: #3a352d; --code-bg: #1c1915;
}
* { box-sizing: border-box; }
body {
  margin: 0; font-size: 16px; line-height: 1.6; color: var(--text); background: var(--bg);
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', sans-serif;
}
h1, h2, h3, h4, h5, h6 { color: var(--heading); }
strong, b { color: var(--text-strong); }
a strong, a b { color: inherit; }
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
  display: inline-flex; align-items: center;
  border-radius: 999px; padding: 0.1rem 0.6rem; font-size: 0.75rem; font-weight: 600;
  border: 1px solid var(--border); color: var(--text-soft);
}
.badge.kind-cached { color: #2f81f7; border-color: #2f81f7; }
/* Ecosystem device: the pyx seal drawn in the ecosystem's own colour, holding that ecosystem's mark.
   The two identities merge into one glyph, so the chip stays neutral and never leans on colour alone.
   One `--eco-seal` per ecosystem, inlined so the UI serves no image requests. For a mark that dies on
   a dark background (OCI's navy), `--eco-seal-dark` holds the variant the two dark selectors alias. */
.badge.ecosystem-pypi { --eco-seal: url('data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20viewBox=%220%200%20100%20100%22%3E%3Cpath%20d=%22M50%206%20L88%2028%20L88%2072%20L50%2094%20L12%2072%20L12%2028%20Z%22%20fill=%22none%22%20stroke=%22%233775a9%22%20stroke-width=%226%22%20stroke-linejoin=%22round%22/%3E%3Cg%20transform=%22translate%2827.0,27.0%29%20scale%281.91667%29%22%20fill=%22%233775A9%22%3E%3Cpath%20d=%22M23.922%2013.58v3.912L20.55%2018.72l-.078.055.052.037%203.45-1.256.026-.036v-3.997l-.053-.036-.025.092z%20M23.621%205.618l-3.04%201.107v3.912l3.339-1.215V5.509zM23.92%2013.457V9.544l-3.336%201.215v3.913zM20.47%2014.71V10.8L17.17%2012v3.913zM17.034%2019.996v-3.912l-3.313%201.206v3.912zM17.17%2016.057v3.868l3.314-1.206V14.85l-3.314%201.206zm2.093%201.882c-.367.134-.663-.074-.663-.463s.296-.814.663-.947c.365-.133.662.075.662.464s-.297.814-.662.946z%20M13.225%209.315l.365-.132-3.285-1.197-3.323%201.21.102.037%203.184%201.16zM20.507%2010.664V6.751L17.17%207.965v3.913zM17.058%2011.918V8.005l-3.302%201.202v3.912zM13.643%209.246l-3.336%201.215v3.913l3.336-1.215zM6.907%2013.165l3.322%201.209v-3.913L6.907%209.252z%20M10.34%207.873l3.281%201.193V5.198l-3.28-1.193zM20.507%202.715L17.19%203.922v3.913l3.317-1.207zM16.95%203.903L13.724%202.73l-3.269%201.19%203.225%201.174zM15.365%204.606l-1.624.592v3.868l3.317-1.207V3.991l-1.693.615zm-.391%202.778c-.367.134-.662-.074-.662-.464s.295-.813.662-.946c.366-.133.663.074.663.464s-.297.813-.663.946z%20M10.229%2018.41v-3.914l-3.322-1.209V17.2zM13.678%2017.182v-3.913l-3.371%201.227v3.913z%20M13.756%2017.154l3.3-1.2V12.04l-3.3%201.2zM13.678%2021.217l-3.371%201.227v-3.912h-.078v3.912l-3.322-1.209v-3.913l-.053-.058-.025-.06-3.336-1.21v-3.948l.034.013%203.287%201.196.015-.078-3.261-1.187%203.26-1.187v-.109L3.876%209.62l-.307-.112%203.26-1.188v.877l.079-.055V6.769l3.257%201.185.058-.061L7.084%206.75l-.102-.037%203.24-1.179v-.083L6.854%206.677v.018l-.025.018v1.523L3.44%209.47v.02l-.025.017v4.007l-3.39%201.233v.019L0%2014.784v3.995l.025.037%203.4%201.237.008-.006.007.01%203.4%201.238.008-.006.006.01%203.4%201.237.014-.009.012.01%203.45-1.256.026-.037-.078-.027zM3.493%209.563l3.257%201.185-3.257%201.187V9.562zM3.4%2019.96L.078%2018.752v-3.913l2.361.86.96.349v3.913zm.015-3.99L.335%2014.85l-.182-.066%203.262-1.187v2.374zm3.399%205.231l-3.321-1.209v-3.912l3.321%201.209v3.912zM23.791%205.434l-3.21-1.17v2.338zM20.387%202.643l-3.24-1.18-3.27%201.19%203.247%201.182z%22/%3E%3C/g%3E%3C/svg%3E'); }
.badge.ecosystem-oci { --eco-seal: url('data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20viewBox=%220%200%20100%20100%22%3E%3Cpath%20d=%22M50%206%20L88%2028%20L88%2072%20L50%2094%20L12%2072%20L12%2028%20Z%22%20fill=%22none%22%20stroke=%22%232496ed%22%20stroke-width=%226%22%20stroke-linejoin=%22round%22/%3E%3Cg%20transform=%22translate%2827.0,27.0%29%20scale%280.0798611%29%22%3E%3Cpolygon%20fill=%22%23808184%22%20points=%22326.6,212.6%20326.6,132.6%20128.6,132.6%20128.6,444.6%20326.6,444.6%20326.6,364.6%20208.6,364.6%20208.6,212.6%22/%3E%20%3Crect%20fill=%22%23262261%22%20x=%22366.5%22%20y=%22132.6%22%20width=%2279.9%22%20height=%2279.9%22/%3E%20%3Crect%20fill=%22%23262261%22%20x=%22366.5%22%20y=%22252.6%22%20width=%2279.9%22%20height=%22192%22/%3E%20%3Cpath%20fill=%22%23262261%22%20d=%22M8.5,9.5v558.2h558.2V9.5H8.5z%20M486.4,484.7H88.7V92.6h397.8V484.7z%22/%3E%3C/g%3E%3C/svg%3E'); --eco-seal-dark: url('data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20viewBox=%220%200%20100%20100%22%3E%3Cpath%20d=%22M50%206%20L88%2028%20L88%2072%20L50%2094%20L12%2072%20L12%2028%20Z%22%20fill=%22none%22%20stroke=%22%232496ed%22%20stroke-width=%226%22%20stroke-linejoin=%22round%22/%3E%3Cg%20transform=%22translate%2827.0,27.0%29%20scale%280.0798611%29%22%3E%3Cpolygon%20fill=%22%23ffffff%22%20points=%22326.6,212.6%20326.6,132.6%20128.6,132.6%20128.6,444.6%20326.6,444.6%20326.6,364.6%20208.6,364.6%20208.6,212.6%22/%3E%20%3Crect%20fill=%22%23ffffff%22%20x=%22366.5%22%20y=%22132.6%22%20width=%2279.9%22%20height=%2279.9%22/%3E%20%3Crect%20fill=%22%23ffffff%22%20x=%22366.5%22%20y=%22252.6%22%20width=%2279.9%22%20height=%22192%22/%3E%20%3Cpath%20fill=%22%23ffffff%22%20d=%22M8.5,9.5v558.2h558.2V9.5H8.5z%20M486.4,484.7H88.7V92.6h397.8V484.7z%22/%3E%3C/g%3E%3C/svg%3E'); }
.badge[class*='ecosystem-'] { color: var(--text-soft); border-color: var(--border); }
.badge[class*='ecosystem-']::before {
  content: ''; display: inline-block; width: 1.25rem; height: 1.25rem;
  background: var(--eco-seal) center/contain no-repeat; margin-right: 0.35rem;
}
:root[data-theme='dark'] .badge.ecosystem-oci { --eco-seal: var(--eco-seal-dark); }
@media (prefers-color-scheme: dark) {
  :root:not([data-theme='light']) .badge.ecosystem-oci { --eco-seal: var(--eco-seal-dark); }
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
