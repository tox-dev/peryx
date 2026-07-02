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
.theme-toggle {
  border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: var(--text-soft);
  width: 2rem; height: 2rem; cursor: pointer; font-size: 0.95rem; line-height: 1;
}
.theme-toggle:hover { border-color: var(--accent); color: var(--accent); }
main { max-width: 70rem; margin: 0 auto; padding: 2rem 1.25rem 4rem; }
.page h1 { letter-spacing: -0.02em; margin-top: 0; }
.page h2 { margin-top: 2rem; border-bottom: 1px solid var(--border); padding-bottom: 0.3rem; }
.dim { color: var(--text-soft); }
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
.badge.uploads { background: linear-gradient(120deg, var(--brand-a), var(--brand-b)); color: #fff; border: none; }
.badge.yanked-badge { color: #e5484d; border-color: #e5484d; }
.badge.meta-badge { color: #34c496; border-color: #34c496; }
.layers code { margin-right: 0.3rem; }
.search, .token {
  width: 100%; max-width: 28rem; padding: 0.55rem 0.9rem; margin: 0.75rem 0 1rem;
  border: 1px solid var(--border); border-radius: 9px; background: var(--bg); color: var(--text);
  font-size: 0.95rem;
}
.search:focus, .token:focus { outline: 2px solid color-mix(in srgb, var(--brand-a) 45%, transparent); }
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
.description :is(h1, h2, h3) { border: none; }
.description pre {
  background: var(--terminal-bg); color: var(--terminal-text); border-radius: 10px; padding: 1rem 1.2rem;
  overflow-x: auto;
}
.description pre code { background: none; color: inherit; padding: 0; }
.description img { max-width: 100%; }
.description-plain { white-space: pre-wrap; }
table.files, table.admin-table { border-collapse: collapse; width: 100%; font-size: 0.92rem; }
table.files th, table.files td, table.admin-table td {
  border: 1px solid var(--border); padding: 0.45rem 0.7rem; text-align: left;
}
table.files th { background: var(--bg-soft); }
tr.yanked td a { text-decoration: line-through; color: var(--text-soft); }
.project-side h3 { margin-bottom: 0.3rem; border-bottom: 1px solid var(--border); padding-bottom: 0.2rem; }
.chips code { margin: 0 0.3rem 0.3rem 0; display: inline-block; }
.classifiers { list-style: none; padding: 0; margin: 0 0 0.6rem; color: var(--text-soft); font-size: 0.85rem; }
.classifier-group { margin: 0.5rem 0 0.1rem; font-weight: 600; font-size: 0.85rem; }
.member-content {
  background: var(--terminal-bg); color: var(--terminal-text); border-radius: 10px; padding: 1rem 1.2rem;
  overflow-x: auto; font-size: 0.85rem;
}
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
