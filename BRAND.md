# Footorrent

**Small footprint. Big downloads.**

The product name is **Footorrent**. The command stays `ftc`, the crates stay
`ftc-core` / `free-torrent-cli`, and the release archives keep their names, so
nothing anyone typed before stops working. Say it *FOOT-torrent*.

## The idea

Footorrent is a BitTorrent client that fits in about 3 MB and carries its own
weight: the wire protocol, trackers, DHT, metadata exchange and the UI are all
in one small binary. The name is the whole pitch — a *footprint* small enough to
forget about, doing the work of something much bigger.

The mark is a footprint whose sole is a download arrow: four toes are the
peers in the swarm, the arrow is the transfer. It reads at 16 px in a tab and at
1024 px as an app icon.

## Palette

Warm paper, ink and one blue. These are the values in `crates/cli/src/ui.html`,
the page the macOS app puts in its window; everything else follows from them.

| Token | Hex | Use |
| --- | --- | --- |
| `--bg` | `#F7F2E8` | window and page background |
| `--panel` | `#FBF7EE` | title bar, cards, dialogs, table header |
| `--panel-2` | `#F0E9DB` | buttons, chips, secondary fills |
| `--panel-3` | `#E8DFCE` | hover state for those controls |
| `--field` | `#FFFDF7` | inputs and table body (set inline in the window) |
| `--line` | `#E3D9C6` | hairlines, card and table borders |
| `--line-2` | `#D2C5AE` | borders that have to hold a shape |
| `--text` | `#1C2A3A` | primary text, headings, numbers |
| `--muted` | `#5B6B7D` | body and secondary text |
| `--dim` | `#8492A0` | labels, placeholders, timestamps |
| `--accent` | `#2F6FA8` | the mark, links, labels, the primary action |
| `--accent-2` | `#5B93C7` | top stop of the button and progress gradients |
| `--ok` | `#2E7D5B` | completed, connected |
| `--warn` | `#A5741C` | paused, warning |
| `--err` | `#B4473F` | failed, and nothing else |
| `--info` | `#3C74A8` | peer and metadata messages |

Blue on paper is the signature pairing. One accent element per view — the Add
Download button — everything else is neutral so status colours stay meaningful:
if the window is red, something failed. `--dim` is for labels and placeholders,
never for body copy; body text is `--muted` or `--text`.

Depth is drawn with hairlines, not shadows. A panel is `--panel` plus a 1 px
`--line` border; shadows appear only under things that float (dialogs, the
terminal and screenshot frames on the site).

## Type

System stacks only, because the app ships no fonts: `-apple-system,
BlinkMacSystemFont, "Segoe UI", Inter, system-ui, sans-serif`. Code and
numerals in tables use `ui-monospace, SFMono-Regular, Menlo, monospace`.

- Wordmark: lowercase `footorrent`, weight 650, tracking `-0.02em`. `foot` is
  `--accent`, `orrent` is `--text`. The title bar appends `Download Manager` in
  `--dim`.
- Headings: 600, tracking `-0.01em` (display sizes up to `-0.035em`).
- Body: 400 at 13–15 px, line height 1.45.
- Numerals are tabular in the download table so sizes and speeds line up.

## Logo

Files in `assets/brand/`:

| File | Use |
| --- | --- |
| `footorrent-mark.svg` | 64 px tile: favicon, app icon, avatars |
| `footorrent-lockup.svg` | mark + wordmark, horizontal |
| `icon-1024.svg` | macOS / store icon source |
| `AppIcon.icns` | generated from `icon-1024.svg`; packed into `Footorrent.app` |

In the window the mark is drawn in `--accent` inside a 26 px `#F1E9DA` tile with
a `--line-2` border, sitting beside the wordmark.

`docs/assets/brand/footorrent-mark.svg` is the published copy the site's
favicon points at, because Pages serves `docs/` only — update it whenever the
mark changes. The 32 px favicon in `crates/cli/src/serve.rs` (`/favicon.ico`)
and the data URI in `crates/cli/src/ui.html` are the same mark at tile scale.

Clear space is half a toe radius on all sides. Minimum tile size is 16 px; below
that, drop the carved arrow and keep the footprint silhouette. Do not rotate,
recolor outside the palette, add a gradient to the mark, or place the lockup on
a photograph.

All three are drawn in this palette with transparent backgrounds: a `#F1E9DA`
tile with a `--line-2` border and 0.23 × side radius, the footprint in
`--accent`, and the arrow carved in the tile colour so it reads as a cut-out
rather than a second shape. The lockup puts the 64 px tile beside the wordmark
and the tagline in `--dim`, and is meant to sit on `--bg` or white, not on a
screenshot.

## Voice

Plain, specific, unimpressed with itself. Numbers over adjectives: "3.2 MB",
"1,546,822 bytes", not "blazing fast". The UI says `fetching`, `downloading`,
`completed`; it does not say "Awesome!" or "Oops!". Error messages name the
thing that failed and what to try next.

Writing the name: **Footorrent** in prose, `footorrent` in the wordmark, `ftc`
in code, paths and commands.

## UI tokens

The palette above is the interface contract. `crates/cli/src/ui.html`,
`docs/index.html`, `docs/release-3.5.0.html` and `docs/release-3.6.0.html` share
one `:root` block: the
window's variables as declared, plus `--field` for the fill it sets inline.
Change a value in the window first, then copy it out.

Controls are 32 px high with 8 px radii, chips are pills at 999 px, panels and
dialogs use 10–12 px radii, and the primary button is a `--accent-2` →
`--accent` vertical gradient with a `#245A8A` border. Progress bars use the same
gradient; completed is `--ok`, paused is `--warn`, stopped and failed stay
neutral and `--err`.
