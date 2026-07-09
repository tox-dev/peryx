+++
title = "Brand"
description = "The peryx identity: the falcon mark, the rust-to-amber gradient, typography, voice, and how to use them."
sort_by = "weight"
template = "section.html"
weight = 50

[extra]
logos = ["mark.svg"]
+++

peryx is **fast as the falcon, sealed as the pyx**: one blazing-fast, open-source vault for a wide range of ecosystems.
The name welds two roots. *per-*, the peregrine falcon, the fastest animal alive and a traveller across every land
(Latin *peregrinus*), for speed and reach. *-yx*, the *pyx*, a sealed box whose contents are assayed for authenticity
(the medieval *Trial of the Pyx*), for the store and its verification. Say it **PERR-iks**; always lowercase.

## Logo

The mark is a peregrine seen head-on in a full stoop, wings raised, diving. It was traced from a photograph, mirrored
for symmetry, and reduced to one gradient silhouette.

<p style="display:flex;gap:1rem;align-items:center">
  <img src="/mark.svg" alt="peryx mark" width="72" height="72">
  <img src="/seal.svg" alt="peryx seal" width="72" height="72">
  <img src="/lockup.svg" alt="peryx lockup" height="56">
</p>

Two forms share one silhouette:

- **Mark** ([mark.svg](/mark.svg), [mark-mono.svg](/mark-mono.svg)) is the everyday logo: favicon, tab, nav, loading,
  CLI. Minimum 16 px, and the only form used below 32 px.
- **Seal** ([seal.svg](/seal.svg)) is the mark inside the hexagonal pyx. Use it where the logo is masked or must read as
  "authentic": the app icon ([icon.svg](/icon.svg)), a verified-artifact badge, print. Minimum 32 px.

Give the mark clear space of half its height. Do not rotate, skew, recolour, add effects, or set the gradient on a busy
field. Never enclose the wordmark in the pyx. The wordmark is lowercase, weight 800, tracking −2%.

## Colour

One bold element, the rust-to-amber gradient, over neutral graphite.

| Token                       | Hex                     | RGB                  |
| --------------------------- | ----------------------- | -------------------- |
| `--brand-a`                 | `#f74c00`               | 247 76 0             |
| `--brand-b`                 | `#ffb600`               | 255 182 0            |
| `--accent` (light / dark)   | `#d94400` / `#ff8a3d`   | 217 68 0 / 255 138 61 |

The signature gradient is `linear-gradient(115deg, #f74c00, #ffb600)`, the same direction on every mark.

## Typography

System stacks, no web fonts. A sans for UI and prose (`system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`); a
mono for labels, data, and the CLI (`ui-monospace, "SF Mono", Menlo, Consolas, monospace`).

## Voice

Concise, technical, plain. Describe what it does and let the numbers carry the boast. Active voice; a control says what
happens. No enterprise buzzwords, no exclamation marks, no jargon a newcomer cannot decode.

## Motion

The mark has one move: the stoop. It folds in from up and back, accelerates like gravity, and settles in about 0.7 s,
then holds still. The seal never animates, and `prefers-reduced-motion` paints everything settled.

## Accessibility

Built to pass WCAG 2.1 AA: body text ≥ 9:1, headings ≥ 15:1, accent and links ≥ 4.5:1 on both themes. Colour never
carries meaning alone, every control has a visible focus ring, and reduced motion is honoured.

## Assets and full guidelines

The logo files sit at the site root: [mark.svg](/mark.svg), [mark-mono.svg](/mark-mono.svg), [lockup.svg](/lockup.svg),
[lockup-stacked.svg](/lockup-stacked.svg), [seal.svg](/seal.svg), [icon.svg](/icon.svg). The complete interactive brand
book, with the full ecosystem colour palette, UI components, motion, and terminal banners, is at
[brand-book.html](/brand-book.html).
