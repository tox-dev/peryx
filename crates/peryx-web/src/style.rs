//! The UI stylesheet, inlined into the page shell.
//!
//! Mirrors the documentation site's design tokens (brand gradient, light/dark palettes,
//! terminal-style code) so the served UI and the docs read as one product.

pub const CSS: &str = r"
:root {
  --bg: #f7f4ef; --bg-soft: #fffdf9; --bg-sink: #efeae1; --text: #1a1a1a; --heading: #111111; --text-strong: #111111; --text-soft: #3f3d3b;
  --text-faint: #6b6862; --accent: #a83600; --accent-strong: #8a2c00; --brand-a: #f74c00; --brand-b: #ffb600;
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
.ops-endpoint { white-space: nowrap; }
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
.badge[class*='ecosystem-'] { color: var(--text-soft); border-color: var(--border); }
.badge.kind-hosted { color: var(--ok); border-color: var(--ok); }
.badge.kind-virtual { color: var(--accent); border-color: var(--accent); }
.badge.source-uploaded { color: var(--ok); border-color: var(--ok); }
.badge.source-cached { color: #2f81f7; border-color: #2f81f7; }
.badge.source-override { color: #8b5cf6; border-color: #8b5cf6; }
.badge.available-local { color: var(--ok); border-color: var(--ok); }
.badge.available-remote { color: var(--text-soft); border-color: var(--border); }
.badge.uploads { background: linear-gradient(115deg, var(--brand-a), var(--brand-b)); color: #fff; border: none; }
/* #550 availability topology: role and node-health chips. Colour only reinforces the word each chip
   already spells out, so an unknown or restricted peer never reads as healthy on colour alone. */
.badge.role-writer { color: var(--accent); border-color: var(--accent); }
.badge.role-replica { color: #2f81f7; border-color: #2f81f7; }
.badge.topology-self { color: var(--ok); border-color: var(--ok); }
.badge.health-live { color: var(--ok); border-color: var(--ok); }
.badge.health-unready { color: var(--warn); border-color: var(--warn); }
.badge.health-unknown { color: var(--text-soft); border-color: var(--border-strong); }
.badge.health-restricted { color: var(--text-soft); border-color: var(--border); font-style: italic; }
.topology-summary, .topology-local { margin: 0.6rem 0; }
.topology-filters { margin-bottom: 0.4rem; }
/* #551 placement health: whole-store availability counts over a paged per-digest table. */
.placement-summary { margin: 0.6rem 0; }
.placement-pager { display: flex; align-items: center; gap: 0.6rem; flex-wrap: wrap; margin-top: 0.6rem; }
.placement-pager .result-count { margin: 0; }
.digest-drill { background: none; border: none; padding: 0; margin: 0; font: inherit; color: inherit; cursor: pointer; text-align: left; }
.digest-drill:hover code, .digest-drill:focus-visible code { text-decoration: underline; }
.placement-detail { margin-top: 0.6rem; }
.virtual-card { grid-column: span 2; }
.card-usage { display: flex; gap: 0.8rem; font-size: 0.85rem; color: var(--text-soft); margin-top: 0.5rem; }
.card-usage a { margin-left: auto; }
.stats-table td { font-variant-numeric: tabular-nums; }
.search, .token {
  width: 100%; max-width: 28rem; padding: 0.55rem 0.9rem; margin: 0.75rem 0 1rem;
  border: 1px solid var(--border); border-radius: 9px; background: var(--bg); color: var(--text);
  font-size: 0.95rem;
}
.search:focus, .token:focus { outline: 2px solid color-mix(in srgb, var(--brand-a) 45%, transparent); }
.upload-form {
  display: grid; grid-template-columns: minmax(8rem, 12rem) minmax(16rem, 34rem); gap: 0.8rem 1rem;
  align-items: center; max-width: 48rem;
}
.upload-form label { font-weight: 600; }
.upload-form select, .upload-form input[type='file'] {
  min-width: 0; border: 1px solid var(--border); border-radius: 9px; background: var(--bg); color: var(--text);
  padding: 0.55rem 0.7rem;
}
.upload-form .token { max-width: none; margin: 0; }
.upload-form .dim, .upload-actions, .upload-form progress, .upload-outcome { grid-column: 2; margin: 0; }
.upload-actions { display: flex; gap: 0.6rem; }
.upload-actions button {
  border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: var(--accent);
  padding: 0.45rem 0.9rem; cursor: pointer; font-weight: 600;
}
.upload-actions button:disabled { color: var(--text-faint); cursor: default; }
.upload-form progress { width: 100%; accent-color: var(--accent); }
.upload-outcome { min-height: 1.6rem; color: var(--text-soft); }
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
.search-results { min-width: 64rem; }
.search-results td:last-child { color: var(--text-soft); min-width: 16rem; }
.pagination { display: flex; align-items: center; gap: 0.75rem; margin-top: 1rem; }
.pagination button, .policy-filters button {
  border: 1px solid var(--border); border-radius: 7px; padding: 0.45rem 0.8rem;
  background: var(--bg); color: var(--accent); cursor: pointer; font-weight: 600;
}
.pagination button:disabled, .policy-filters button:disabled { color: var(--text-faint); cursor: default; }
.policy-filters {
  display: grid; grid-template-columns: repeat(4, minmax(10rem, 1fr)); gap: 0.45rem 0.8rem;
  align-items: end; margin: 1.25rem 0;
}
.policy-filters label { font-size: 0.82rem; font-weight: 600; color: var(--text-soft); }
.policy-filters :is(input, select) {
  min-width: 0; border: 1px solid var(--border); border-radius: 7px; padding: 0.5rem 0.65rem;
  background: var(--bg); color: var(--text);
}
.policy-filters :is(input, select, button):focus-visible { outline: 3px solid var(--accent); outline-offset: 2px; }
.policy-results { min-height: 4rem; }
.policy-decisions-table { min-width: 88rem; }
.policy-decisions-table caption { text-align: left; padding: 0 0 0.6rem; color: var(--text-soft); }
.decision-allow { color: var(--ok); }
.decision-deny { color: var(--bad); }
.decision-wait { color: var(--warn); }
.shadow-inspection-table { min-width: 82rem; }
.shadow-inspection-table caption { text-align: left; padding: 0 0 0.6rem; color: var(--text-soft); }
.badge.outcome-selected { color: var(--ok); border-color: var(--ok); }
.badge.outcome-shadowed { color: var(--text-soft); border-color: var(--border-strong); }
.trash-table { min-width: 88rem; }
.trash-table caption { text-align: left; padding: 0 0 0.6rem; color: var(--text-soft); }
.badge.trash-restorable { color: var(--ok); border-color: var(--ok); }
.badge.trash-expired { color: var(--text-soft); border-color: var(--border); }
.analytics-results { min-height: 4rem; }
.usage-table { min-width: 44rem; }
.usage-table caption { text-align: left; padding: 0 0 0.6rem; color: var(--text-soft); }
.usage-table :is(td, th).num { text-align: right; font-variant-numeric: tabular-nums; }
.usage-interval { color: var(--text-soft); margin: 0 0 0.4rem; }
.usage-interval strong { color: var(--text); }
.usage-retention { color: var(--warn); border-left: 3px solid var(--warn); padding-left: 0.6rem; margin: 0 0 0.8rem; }
.page-link {
  border: 1px solid var(--border); border-radius: 7px; padding: 0.3rem 0.75rem; color: var(--accent);
}
.page-link:hover { border-color: var(--accent); text-decoration: none; }
.page-link.disabled { color: var(--text-soft); background: var(--bg-soft); }
.breadcrumb { color: var(--text-soft); font-size: 0.9rem; }
.browse-head { margin-bottom: 1rem; }
.browse-subtitle { color: var(--text-soft); margin-top: -0.7rem; }
.browse-badges { display: flex; flex-wrap: wrap; gap: 0.35rem; }
.browse-section { margin-top: 1.5rem; }
.browse-properties { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: 0.35rem 1rem; }
.browse-properties dt { color: var(--text-soft); font-weight: 600; }
.browse-properties dd { margin: 0; }
.browse-table { border-collapse: collapse; width: 100%; }
.browse-table th, .browse-table td { border: 1px solid var(--border); padding: 0.45rem 0.7rem; text-align: left; }
.browse-table th { background: var(--bg-soft); }
.browse-content { background: var(--terminal-bg); color: var(--terminal-text); padding: 1rem; overflow-x: auto; }
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
@media (max-width: 52rem) {
  .site-header nav { flex-wrap: wrap; }
  .header-search { order: 3; flex-basis: 100%; max-width: none; }
  .nav-links { flex: 1 1 100%; flex-wrap: wrap; justify-content: flex-end; margin-left: auto; }
  .search-controls { grid-template-columns: 1fr 1fr; }
  .search-controls .search { grid-column: 1 / -1; }
  .upload-form { grid-template-columns: 1fr; }
  .upload-form .dim, .upload-actions, .upload-form progress, .upload-outcome { grid-column: 1; }
  .policy-filters { grid-template-columns: 1fr 1fr; }
}
.description :is(h1, h2, h3) { border: none; }
.description pre {
  background: var(--terminal-bg); color: var(--terminal-text); border-radius: 10px; padding: 1rem 1.2rem;
  overflow-x: auto;
}
.description pre code { background: none; color: inherit; padding: 0; }
.description img { max-width: 100%; }
.description-plain { white-space: pre-wrap; }
.chips code { margin: 0 0.3rem 0.3rem 0; display: inline-block; }
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
