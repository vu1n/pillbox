# Pillbox brand

Vector logo + favicon assets used by the README and any frontend. The full kit
(glossy 3D icon, PNG favicons, GitHub social card, README header banner) is
distributed separately.

## Files
- `pillbox-logo-{light,dark}.png` — full lockup (isometric icon + stylized wordmark). The README uses these via `<picture>`.
- `pillbox-icon-{light,dark}.png` — isometric mark only (avatars, app icon, square spots).
- `pillbox-wordmark-{light,dark}.png` — stylized wordmark only.
- `pillbox-glyph.svg` / `pillbox-glyph-dark.svg` — simplified flat icon, vector (favicons, tiny UI).
- `favicon.svg` — theme-adaptive favicon (box recolors in dark mode; the lid stays Pillbox Blue).

Use the `-light` files on light backgrounds and `-dark` on dark; all PNGs are transparent.

## Palette
| Name | Hex | Use |
|---|---|---|
| Pillbox Blue | `#0A5BFF` | The lid, accents, links |
| Ink | `#0E1116` | Wordmark / box on light |
| Navy | `#0B0F14` | Dark canvas |
| Slate | `#2C374C` | Box on dark backgrounds |
| Paper | `#E9EEF7` | Box / wordmark on dark |

## Type
Wordmark: the stylized **pillbox** lettering (with the blue `i`-dot), baked into the
lockup PNGs. Supporting copy: Poppins. Code / terminal: JetBrains Mono.

## Usage
- Use the dark lockup/glyph on dark backgrounds — never the pure-ink box on dark (it dissolves).
- Keep clear space ≈ the height of the lid.
- Don't stretch, rotate, recolor the lid off-brand, or add your own shadows/gradients.
- The face mark is a single `>_` prompt. Keep it that way.
