+++
title = "Brand"
description = "Rules for the peryx mark, colour, typography, voice, and motion."
sort_by = "weight"
template = "section.html"
weight = 50
[extra]
logos = [ "mark.svg"]
+++

{{<brandDefs />}}

peryx is an open-source artifact service for multiple ecosystems. Its mark is traced from a diving peregrine.

## The name

Two roots form the name and describe its character. Say it **PERR-iks** and write it in lowercase.

- **per· · the peregrine:** the peregrine reaches over 380 km/h in a dive. Its Latin root, *peregrinus*, means "from
  every land".
- **·yx · the pyx:** a sealed, assayed vault. A *pyx* is a sealed box for safekeeping. During the *Trial of the Pyx*,
  officials lock sample coins away and assay them to prove authenticity. Artifact verification follows that model.

## What peryx is

peryx is MIT-licensed and self-hosted. One binary serves every configured role. Configuration activates only the
selected ecosystem owners and availability mode; inactive implementations add no runtime work.

## Voice & taglines

Use concise, technical language. State behavior directly. Controls name the action they perform.

| Register | Line                                                  |
| -------- | ----------------------------------------------------- |
| Lead     | Artifact storage across ecosystems.                   |
| Sub      | Cache upstream artifacts and host private artifacts.  |
| Dev      | One binary with protocol behavior for each ecosystem. |
| Security | Verify artifact digests before storage and service.   |

Avoid enterprise buzzwords, "revolutionary", exclamation marks, and jargon that a newcomer cannot decode.

## The logo

The mark shows a peregrine head-on in a full stoop with raised wings. We traced a photograph, mirrored it for symmetry,
and reduced it to one gradient silhouette.

<div class="brand-logo-hero">
  <div><div class="brand-tile brand-tile-dark">{{<falcon label="peryx mark" />}}</div><p class="brand-capt">gradient · dark</p></div>
  <div><div class="brand-tile brand-tile-light">{{<falcon label="peryx mark" />}}</div><p class="brand-capt">gradient · light</p></div>
</div>

<div class="brand-origin">
  <span class="brand-frame">{{<falcon label="the mark" />}}</span>
  <span class="brand-arrow">→</span>
  <span class="brand-frame"><img src="../seal.svg" alt="the pyx seal" width="56" height="56"></span>
</div>

Use the bare falcon as the standard mark. Inside the hexagonal pyx, it becomes the app icon and verified-artifact badge.
The enclosure survives OS icon masks that clip the bare mark.

### Clear space & minimum size

<div class="brand-clearbox"><div class="brand-cs"><span class="brand-guide"></span>{{<falcon label="peryx mark with clear space" />}}</div></div>

Keep clear space of half the mark's height on all sides. Minimum size 16 px, where it still reads as the falcon.

## Logo expressions

The shipped forms use one path and gradient. The interactive brand book, with copy-to-clipboard swatches and the live
ecosystem palette, is at [the brand book](../brand-book/).

### Wordmark lockups

<div class="brand-lockups">
  <div class="brand-lockcell"><img src="../lockup.svg" alt="peryx horizontal lockup" height="44"></div>
  <div class="brand-lockcell"><img src="../lockup-stacked.svg" alt="peryx stacked lockup" height="76"></div>
</div>

Use lowercase, weight 800, and -2% tracking. Use the gradient at display sizes and solid `--text` in body text and
navigation. Keep the wordmark outside the pyx.

### Sizes, single-colour & seal

<div class="brand-expr">
  <div class="brand-ex"><span class="brand-appicon"><img src="../seal.svg" alt="" width="44" height="44"></span><span class="brand-capt">app icon · seal</span></div>
  <div class="brand-ex"><span class="brand-appicon">{{<falcon />}}</span><span class="brand-capt">avatar · mark</span></div>
  <div class="brand-ex"><span class="brand-fav brand-fav-32">{{<falcon />}}</span><span class="brand-capt">32px</span></div>
  <div class="brand-ex"><span class="brand-fav brand-fav-16">{{<falcon />}}</span><span class="brand-capt">16px</span></div>
  <div class="brand-ex"><span class="brand-monocell">{{<falcon mono={true} />}}</span><span class="brand-capt">mono</span></div>
</div>

## Motion

The mark uses one motion, the stoop. It folds in from above, accelerates, emits speed streaks, and settles in about 0.7
s. Click to replay. Keep the seal static.

{{<brandMotion />}}

Animate transform and opacity. With `prefers-reduced-motion`, paint the mark in its settled position, hold the loop, and
show progress-bar values without animation.

## Colour

One bold element, the ember-to-amber gradient, over neutral graphite. The signature gradient is
`linear-gradient(115deg, #f74c00, #ffb600)`, the same direction on every mark.

<div class="brand-gradient" aria-hidden="true"></div>

| Token             | Hex                   | RGB                   |
| ----------------- | --------------------- | --------------------- |
| `--brand-a`       | `#f74c00`             | 247 76 0              |
| `--brand-b`       | `#ffb600`             | 255 182 0             |
| `--accent`        | `#d94400` / `#ff8a3d` | 217 68 0 / 255 138 61 |
| `--accent-strong` | `#b23800`             | 178 56 0              |

Light and dark share one palette: ink `#1c2026` on paper `#ffffff`, mist `#9aa4b0` on night `#12151a`.

Semantic colours signal **state**. Keep the gradient for brand elements, and pair each colour with another signal.

| State       | Hex       |
| ----------- | --------- |
| Faster / OK | `#0ca30c` |
| Warn        | `#d98a00` |
| Slow        | `#ec835a` |
| Critical    | `#d03b3b` |

## Ecosystems

Use a coloured device for each ecosystem and keep it **off the mark**. Use the ecosystem's brand colour so owner
identities remain distinct from the peryx gradient. A green dot marks owners compiled into peryx.

{{<ecosystems />}}

## Typography

System stacks: a mono for labels, data, and the CLI the tool lives in, and a sans for UI and prose. No web fonts. The
sans is `system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`; the mono is
`ui-monospace, "SF Mono", Menlo, Consolas, monospace`.

<div class="brand-typerow"><span class="brand-typelabel">Display / wordmark: sans 800, -2% tracking</span><span class="brand-type-display">peryx</span></div>
<div class="brand-typerow"><span class="brand-typelabel">Heading: sans 700</span><span class="brand-type-h">Cache and host artifacts across ecosystems.</span></div>
<div class="brand-typerow"><span class="brand-typelabel">Body: sans 400</span><span class="brand-type-body">A virtual index composes cached and hosted resources under one route.</span></div>
<div class="brand-typerow"><span class="brand-typelabel">Mono: labels, code, CLI</span><span class="brand-type-mono">$ peryx config check --config peryx.toml <span class="brand-dim"># valid</span></span></div>

## In product

Buttons, badges, and status use the gradient and neutral palette.

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
      <span class="brand-badge brand-badge-verified"><span class="brand-badge-mk">{{<falcon mono={true} />}}</span>Verified</span>
      <span class="brand-badge brand-badge-ok"><span class="brand-badge-dot"></span>Healthy</span>
      <span class="brand-badge">cached</span>
      <span class="brand-badge">overridden</span>
    </div>
  </div>
</div>

<div class="brand-comp">
  <div class="brand-compcard">
    <div class="brand-k">Ecosystem tags</div>
    {{<ecosystemChips />}}
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
      <span class="brand-wbrand">{{<falcon />}}<span class="brand-wtitle">peryx</span></span>
      <span class="brand-wlinks"><span>Docs</span><span>Ecosystems</span><span>Pricing</span></span>
      <span class="brand-wcta">Get started</span>
    </div>
    <div class="brand-capt">website header</div>
  </div>
  <div class="brand-wcard">
    <div class="brand-wrow">
      <span class="brand-wavatar brand-wavatar-sq"><img src="../seal.svg" alt="" width="34" height="34"></span>
      <span class="brand-wavatar">{{<falcon />}}</span>
      <span class="brand-wbadge"><span class="brand-wbadge-k">peryx</span><span class="brand-wbadge-v">verified</span></span>
    </div>
    <div class="brand-capt">app icon · social avatar · README badge</div>
  </div>
</div>

## Startup banner

The service prints the startup logo at boot. Choose one of two terminal variants at runtime; suppress both for non-TTY
output and CI.

### Modern terminals: truecolor and Unicode blocks

<div class="brand-terminal"><div class="brand-terminal-bar"><span></span><span></span><span></span></div><pre><span class="brand-banner-grad" aria-hidden="true">  ██████  ███████ ██████  ██   ██ ██   ██
  ██   ██ ██      ██   ██  ██ ██   ██ ██
  ██████  █████   ██████    ███     ███
  ██      ██      ██   ██    ██    ██ ██
  ██      ███████ ██   ██    ██   ██   ██</span>
   <span class="brand-dim">the artifact vault · v0.1.0</span>

<span class="brand-g">→</span> proxy <span class="brand-dim">upstream.example</span> <span class="brand-g">→</span>
hosted <span class="brand-dim">2,481 resources · 6.2 GB</span> <span class="brand-g">→</span> virtual
<span class="brand-dim">/cache, /hosted</span> <span class="brand-ok">✓</span> ready in
<span class="brand-ok">0.42s</span> on <span class="brand-ok">:8080</span></pre></div>

### Old terminals: ASCII, 16-colour, or mono

<div class="brand-terminal"><div class="brand-terminal-bar"><span></span><span></span><span></span></div><pre><span aria-hidden="true">   _ __   ___ _ __ _   ___  __
  | '_ \ / _ \ '__| | | \ \/ /
  | |_) |  __/ |  | |_| |&gt;  &lt;
  | .__/ \___|_|   \__, /_/\_\
  |_|              |___/</span>
  the artifact vault   v0.1.0
  ------------------------------------
  <span class="brand-g">-&gt;</span> proxy    upstream.example
  <span class="brand-g">-&gt;</span> hosted   2481 resources, 6.2 GB
  <span class="brand-g">-&gt;</span> virtual  /cache, /hosted
  [<span class="brand-ok">ok</span>] ready in 0.42s on :8080</pre></div>

Pick at runtime: truecolor plus UTF-8 selects the modern build; an older `TERM`, `NO_COLOR`, or a non-TTY pipe drops to
the ASCII build in the terminal's own foreground.

## Accessibility

The system targets WCAG 2.1 AA. Its requirements cover contrast, focus, motion, and language.

| Surface (measured)     | Contrast |
| ---------------------- | -------- |
| Body text · light      | 9.9:1    |
| Body text · dark       | 9.9:1    |
| Headings · both        | 15:1     |
| Secondary labels       | ≥ 5:1    |
| Accent / links · light | 4.7:1    |

Each control shows a visible focus ring, and interactive demos support Tab + Enter. `prefers-reduced-motion` stops the
dive, loop, and progress bars. Ecosystem indicators pair a dot with a name; status indicators pair an icon with a word.
Use a 16 px minimum for the mark and body text, and 32 px for the seal. Write in active voice with familiar terms.

## Usage

Apply these examples to keep the identity consistent.

<div class="brand-dd">
  <div class="brand-ddc brand-good"><div class="brand-stage">{{<falcon />}}</div><div class="brand-capt"><span class="brand-mark-good">✓</span> Gradient mark on a clean, contrasting ground.</div></div>
  <div class="brand-ddc brand-bad"><div class="brand-stage" style="background:#7a4b2a">{{<falcon />}}</div><div class="brand-capt"><span class="brand-mark-bad">×</span> Gradient on a busy or low-contrast field.</div></div>
  <div class="brand-ddc brand-good"><div class="brand-stage"><img src="../seal.svg" alt="" width="56" height="56"></div><div class="brand-capt"><span class="brand-mark-good">✓</span> Pyx seal for app tiles, badges, anything masked.</div></div>
  <div class="brand-ddc brand-bad"><div class="brand-stage"><span class="brand-skewed">{{<falcon />}}</span></div><div class="brand-capt"><span class="brand-mark-bad">×</span> Do not rotate, skew, or recolour the mark.</div></div>
</div>
