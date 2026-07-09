+++
title = "Brand"
description = "The peryx identity: the falcon mark, the rust-to-amber gradient, the ecosystem colours, typography, voice, motion, and how to use them."
sort_by = "weight"
template = "section.html"
weight = 50
[extra]
logos = [ "mark.svg"]
+++

{{ brand_defs() }}

peryx is **fast as the falcon, sealed as the pyx**: one blazing-fast, open-source vault for a wide range of ecosystems.
The identity is built on a mark traced from a photo of a diving peregrine.

## The name

Two real roots make one coined word. Between them they cover the three traits. Say it **PERR-iks**; always lowercase.

- **per· · the peregrine** — speed, and every land. The fastest animal alive, over 380 km/h in a dive. Its Latin root
  *peregrinus* means "from every land", which stands for the wide-ecosystem axis: one vault, artifacts from many
  ecosystems.
- **·yx · the pyx** — a sealed, assayed vault. A *pyx* is a sealed box for safekeeping. In the *Trial of the Pyx*,
  sample coins are locked away, then assayed to prove they are genuine. That is the upload pipeline.

## What peryx is

- **Open source** — free, inspectable, self-hosted. No seat tax to run your own registry.
- **Blazing fast** — features you do not enable cost nothing: no CPU, RAM, or latency for anything switched off.
- **Wide ecosystem support** — PyPI and OCI today; npm, Maven, and Cargo next. Each is a driver behind one model.

## Voice & taglines

Concise, technical, plain. Describe what it does and let the numbers carry the boast. Active voice; a control says what
happens.

| Register | Line                                                           |
| -------- | -------------------------------------------------------------- |
| Lead     | Fast as the falcon, sealed as the pyx.                         |
| Sub      | One blazing-fast vault for a wide range of ecosystems.         |
| Dev      | Catch every package and prove every artifact, at falcon speed. |
| Security | Held under seal. Proven on push. Served at falcon speed.       |

**Voice is not:** enterprise buzzwords, "revolutionary", exclamation marks, or jargon a newcomer cannot decode.

## The logo

A peregrine seen head-on in a full stoop, wings raised, diving. We traced it from a photograph rather than drawing it,
mirrored it for symmetry, and reduced it to one gradient silhouette.

<div class="brand-logo-hero">
  <div><div class="brand-tile brand-tile-dark">{{ falcon(label="peryx mark") }}</div><p class="brand-capt">gradient · dark</p></div>
  <div><div class="brand-tile brand-tile-light">{{ falcon(label="peryx mark") }}</div><p class="brand-capt">gradient · light</p></div>
</div>

<div class="brand-origin">
  <span class="brand-frame">{{ falcon(label="the mark") }}</span>
  <span class="brand-arrow">→</span>
  <span class="brand-frame"><img src="../seal.svg" alt="the pyx seal" width="56" height="56"></span>
</div>

**One silhouette, two uses.** The bare falcon is the everyday mark. Sealed inside the hexagonal pyx it becomes the app
icon and the verified-artifact badge. The enclosure survives the OS icon masks that clip the bare mark.

### Clear space & minimum size

<div class="brand-clearbox"><div class="brand-cs"><span class="brand-guide"></span>{{ falcon(label="peryx mark with clear space") }}</div></div>

Keep clear space of half the mark's height on all sides. Minimum size 16 px, where it still reads as the falcon.

## Logo expressions

Every form the mark ships in, all from one path plus the gradient. The complete interactive brand book, with
copy-to-clipboard swatches and the live ecosystem palette, is at [the brand book](../brand-book/).

### Wordmark lockups

<div class="brand-lockups">
  <div class="brand-lockcell"><img src="../lockup.svg" alt="peryx horizontal lockup" height="44"></div>
  <div class="brand-lockcell"><img src="../lockup-stacked.svg" alt="peryx stacked lockup" height="76"></div>
</div>

Lowercase, weight 800, tracking −2%. Gradient at display sizes, solid `--text` in body and nav. Never enclose the
wordmark in the pyx.

### Sizes, single-colour & seal

<div class="brand-expr">
  <div class="brand-ex"><span class="brand-appicon"><img src="../seal.svg" alt="" width="44" height="44"></span><span class="brand-capt">app icon · seal</span></div>
  <div class="brand-ex"><span class="brand-appicon">{{ falcon() }}</span><span class="brand-capt">avatar · mark</span></div>
  <div class="brand-ex"><span class="brand-fav brand-fav-32">{{ falcon() }}</span><span class="brand-capt">32px</span></div>
  <div class="brand-ex"><span class="brand-fav brand-fav-16">{{ falcon() }}</span><span class="brand-capt">16px</span></div>
  <div class="brand-ex"><span class="brand-monocell">{{ falcon(mono=true) }}</span><span class="brand-capt">mono</span></div>
</div>

## Motion

The mark has one move: the stoop. It folds in from up and back, accelerates like gravity, throws off speed streaks, and
settles in about 0.7 s. Click to replay. The seal never animates.

{{ brand_motion() }}

Transform and opacity only. Motion is a flourish, not a dependency: with `prefers-reduced-motion` the mark paints
settled, the loop holds still, and progress bars show their value without animating.

## Colour

One bold element, the rust-to-amber gradient, over neutral graphite. The signature gradient is
`linear-gradient(115deg, #f74c00, #ffb600)`, the same direction on every mark.

<div class="brand-gradient" aria-hidden="true"></div>

| Token             | Hex                   | RGB                   |
| ----------------- | --------------------- | --------------------- |
| `--brand-a`       | `#f74c00`             | 247 76 0              |
| `--brand-b`       | `#ffb600`             | 255 182 0             |
| `--accent`        | `#d94400` / `#ff8a3d` | 217 68 0 / 255 138 61 |
| `--accent-strong` | `#b23800`             | 178 56 0              |

Light and dark are one palette, not two brands: ink `#1c2026` on paper `#ffffff`, mist `#9aa4b0` on night `#12151a`.

Semantic colours signal **state, not brand**. They never stand in for the gradient, and colour never carries meaning on
its own.

| State       | Hex       |
| ----------- | --------- |
| Faster / OK | `#0ca30c` |
| Warn        | `#d98a00` |
| Slow        | `#ec835a` |
| Critical    | `#d03b3b` |

## Ecosystems

A coloured device carries the ecosystem, and it is kept **off the mark** — the falcon never takes an ecosystem's colour.
Each package type wears its own project's brand colour, so a PyPI index reads as PyPI and an OCI registry reads as OCI
while both sit under one peryx gradient. It maps across the package types Artifactory supports, each in that project's
own brand colour. A green dot marks the ones live in peryx today; the rest are what the `(role × ecosystem)` model is
built to absorb without rework.

{{ ecosystems() }}

## Typography

System stacks: a mono for labels, data, and the CLI the tool lives in, and a sans for UI and prose. No web fonts. The
sans is `system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`; the mono is
`ui-monospace, "SF Mono", Menlo, Consolas, monospace`.

<div class="brand-typerow"><span class="brand-typelabel">Display / wordmark — sans 800, −2% tracking</span><span class="brand-type-display">peryx</span></div>
<div class="brand-typerow"><span class="brand-typelabel">Heading — sans 700</span><span class="brand-type-h">Serve a wide range of ecosystems from one vault.</span></div>
<div class="brand-typerow"><span class="brand-typelabel">Body — sans 400</span><span class="brand-type-body">A caching proxy, a hosted store, and a virtual index that merges the two so local packages override upstream.</span></div>
<div class="brand-typerow"><span class="brand-typelabel">Mono — labels, code, CLI</span><span class="brand-type-mono">$ peryx mirror sync --ecosystem pypi <span class="brand-dim"># 1,284 files · 0.6s</span></span></div>

## In product

The system in use: buttons, badges, and status, all drawn from the gradient and the neutrals.

<div class="brand-comp">
  <div class="brand-compcard">
    <div class="brand-k">Buttons</div>
    <div class="brand-btnrow">
      <button class="brand-btn brand-btn-primary" type="button">Publish</button>
      <button class="brand-btn" type="button">Cancel</button>
      <button class="brand-btn brand-btn-ghost" type="button">Details</button>
    </div>
  </div>
  <div class="brand-compcard">
    <div class="brand-k">Badges &amp; status</div>
    <div class="brand-badges">
      <span class="brand-badge brand-badge-verified"><span class="brand-badge-mk">{{ falcon(mono=true) }}</span>Verified</span>
      <span class="brand-badge brand-badge-ok"><span class="brand-badge-dot"></span>Healthy</span>
      <span class="brand-badge">cached</span>
      <span class="brand-badge">overridden</span>
    </div>
  </div>
</div>

<div class="brand-comp">
  <div class="brand-compcard">
    <div class="brand-k">Ecosystem tags</div>
    {{ ecosystem_chips() }}
  </div>
  <div class="brand-compcard">
    <div class="brand-k">Progress</div>
    <div class="brand-prog brand-prog-det"><span></span></div>
    <div class="brand-prog brand-prog-indet"><span></span></div>
  </div>
</div>

### In the wild

<div class="brand-wild">
  <div class="brand-wcard">
    <div class="brand-wnav">
      <span class="brand-wbrand">{{ falcon() }}<span class="brand-wtitle">peryx</span></span>
      <span class="brand-wlinks"><span>Docs</span><span>Ecosystems</span><span>Pricing</span></span>
      <span class="brand-wcta">Get started</span>
    </div>
    <div class="brand-capt">website header</div>
  </div>
  <div class="brand-wcard">
    <div class="brand-wrow">
      <span class="brand-wavatar brand-wavatar-sq"><img src="../seal.svg" alt="" width="34" height="34"></span>
      <span class="brand-wavatar">{{ falcon() }}</span>
      <span class="brand-wbadge"><span class="brand-wbadge-k">peryx</span><span class="brand-wbadge-v">verified</span></span>
    </div>
    <div class="brand-capt">app icon · social avatar · README badge</div>
  </div>
</div>

## Startup banner

The startup logo prints when the service boots. Two separate builds for two eras of terminal. Pick one at runtime; keep
both off for non-TTY output and CI.

### Modern terminals — truecolor & Unicode blocks

<div class="brand-terminal"><div class="brand-terminal-bar"><span></span><span></span><span></span></div><pre><span class="brand-banner-grad" aria-hidden="true">  ██████  ███████ ██████  ██   ██ ██   ██
  ██   ██ ██      ██   ██  ██ ██   ██ ██
  ██████  █████   ██████    ███     ███
  ██      ██      ██   ██    ██    ██ ██
  ██      ███████ ██   ██    ██   ██   ██</span>
   <span class="brand-dim">the artifact vault · v0.1.0</span>

<span class="brand-g">→</span> proxy <span class="brand-dim">pypi.org, ghcr.io</span> <span class="brand-g">→</span>
hosted <span class="brand-dim">2,481 packages · 6.2 GB</span> <span class="brand-g">→</span> virtual
<span class="brand-dim">/simple, /v2</span> <span class="brand-ok">✓</span> ready in <span class="brand-ok">0.42s</span>
on <span class="brand-ok">:8080</span></pre></div>

### Old terminals — ASCII only, 16-colour or mono

<div class="brand-terminal"><div class="brand-terminal-bar"><span></span><span></span><span></span></div><pre><span aria-hidden="true">   _ __   ___ _ __ _   ___  __
  | '_ \ / _ \ '__| | | \ \/ /
  | |_) |  __/ |  | |_| |&gt;  &lt;
  | .__/ \___|_|   \__, /_/\_\
  |_|              |___/</span>
  the artifact vault   v0.1.0
  ------------------------------------
  <span class="brand-g">-&gt;</span> proxy    pypi.org, ghcr.io
  <span class="brand-g">-&gt;</span> hosted   2481 packages, 6.2 GB
  <span class="brand-g">-&gt;</span> virtual  /simple, /v2
  [<span class="brand-ok">ok</span>] ready in 0.42s on :8080</pre></div>

Pick at runtime: truecolor plus UTF-8 selects the modern build; an older `TERM`, `NO_COLOR`, or a non-TTY pipe drops to
the ASCII build in the terminal's own foreground.

## Accessibility

Built to pass WCAG 2.1 AA. Contrast, focus, motion, and language are part of the system, not an afterthought.

| Surface (measured)     | Contrast |
| ---------------------- | -------- |
| Body text · light      | 9.9 : 1  |
| Body text · dark       | 9.9 : 1  |
| Headings · both        | 15 : 1   |
| Secondary labels       | ≥ 5 : 1  |
| Accent / links · light | 4.7 : 1  |

Beyond contrast:

- **Focus** — every control shows a visible focus ring.
- **Keyboard** — interactive demos run on Tab + Enter.
- **Motion** — `prefers-reduced-motion` stops the dive, loop, and progress bars.
- **Colour never carries meaning alone** — ecosystem is a dot plus its name; status is an icon plus a word.
- **Minimum sizes** — mark 16 px, seal 32 px, body 16 px.
- **Plain language** — active voice, no jargon a newcomer cannot decode.

## Usage

A few rules keep it coherent.

<div class="brand-dd">
  <div class="brand-ddc brand-good"><div class="brand-stage">{{ falcon() }}</div><div class="brand-capt"><span class="brand-mark-good">✓</span> Gradient mark on a clean, contrasting ground.</div></div>
  <div class="brand-ddc brand-bad"><div class="brand-stage" style="background:#7a4b2a">{{ falcon() }}</div><div class="brand-capt"><span class="brand-mark-bad">×</span> Gradient on a busy or low-contrast field.</div></div>
  <div class="brand-ddc brand-good"><div class="brand-stage"><img src="../seal.svg" alt="" width="56" height="56"></div><div class="brand-capt"><span class="brand-mark-good">✓</span> Pyx seal for app tiles, badges, anything masked.</div></div>
  <div class="brand-ddc brand-bad"><div class="brand-stage"><span class="brand-skewed">{{ falcon() }}</span></div><div class="brand-capt"><span class="brand-mark-bad">×</span> Do not rotate, skew, or recolour the mark.</div></div>
</div>
