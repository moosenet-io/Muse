# Muse — brand SVG pack

Constellation-family visual identity for Muse. Deep-indigo ground, violet accent
ramp, dots-and-lines constellation motif rendered as a lyre (celestial + muse).

## Files
| File | Use |
|------|-----|
| `muse-mark.svg` | Primary mark (420x420) — repo avatar, favicon source |
| `muse-lockup.svg` | Horizontal lockup (680x200) — README header, docs |
| `muse-mono.svg` | Monochrome, transparent (200x220) — stamps, small favicons, one-color contexts |
| `muse-banner.svg` | Social/OpenGraph banner (680x356) — **source** for the repo social preview (GitHub accepts PNG/JPG/GIF only, not SVG — use `png/muse-banner-1280.png`) |
| `muse-icons.svg` | Sub-domain glyph set (680x250) — taste, curation, metadata, availability, recommend, channel director |

## Palette (Constellation family)
- Ground: `#14112b` / card `#1a1636`
- Accent ramp: `#534AB7` `#7F77DD` `#AFA9EC` `#CECBF6` `#EEEDFE`
- Peer to Harmony, Chord, Lumina, Terminus.

## Notes
- SVGs use system font stacks (Georgia serif for the wordmark, monospace for labels).
  For pixel-identical rendering across machines, convert the wordmark to outlines,
  or swap in the repo's chosen brand fonts.
- For PNG favicons, rasterize `muse-mark.svg` or `muse-mono.svg` at 512/192/32px.

## Provenance & rights

First-party project artwork: created for the Lumina Constellation by the project
owner (moosenet) and supplied for this repository. Not third-party or
externally-licensed material, so there is no upstream attribution requirement to
carry.

These files are mirrored to the public GitHub repo along with the rest of the
tree. Note the usual distinction: the repository's MIT `LICENSE` covers the
**code**, and permissive code licenses do not grant trademark or brand rights.
Treat the mark and wordmark as project identity — reuse them to refer to Muse,
not to brand something else. If different terms are wanted, state them here and
they override this note.

## Verified safe to publish (2026-07-30)

Checked before the assets went to the public mirror, because SVG is an active
format and README rendering differs between Gitea and GitHub:

- **Inert.** Every element present is one of `svg g rect circle line text title
  desc`; every attribute is geometry/style (`viewBox`, `fill`, `transform`,
  `font-*`, `x`/`y`/`r`/`cx`/`cy`, `opacity`, `stroke*`, `text-anchor`,
  `letter-spacing`, `role`, `xmlns`). No `<script>`, no `on*` event handlers, no
  `javascript:`, no `<foreignObject>`, no `<iframe>`, no `<animate>`/`<set>`.
- **Self-contained.** No external `href`/`xlink:href`/`url()` references, so
  nothing tries to fetch a remote font or image — which would render broken under
  GitHub's README CSP.
- **No PII.** No IPs, hostnames, emails or internal identifiers.

Re-run those checks if the pack is ever regenerated or edited.

## Rasterized PNGs (`png/`)

GitHub and Gitea both need raster images for the places that matter most, so the
two files you actually upload are pre-rendered here:

| File | Use |
|------|-----|
| `png/muse-mark-512.png` | Gitea repo avatar (and any 512px icon slot) |
| `png/muse-mark-192.png` | PWA / apple-touch icon |
| `png/muse-mark-32.png` | favicon |
| `png/muse-banner-1280.png` | GitHub social preview (1280x670) |

Rendered from the SVGs with headless chromium at deviceScaleFactor 1 and
verified visually — the wordmark uses a system serif stack, so a raster made on
a machine without a Georgia-class serif would substitute a different face. If
you regenerate these, check the wordmark actually looks right rather than
trusting the exit code.
