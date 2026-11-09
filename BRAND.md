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

| Token | Hex | Use |
| --- | --- | --- |
| Ink | `#111318` | primary background |
| Graphite | `#1B1E26` | panels, cards |
| Slate | `#2A2F3A` | borders, dividers |
| Paper | `#F5F3EF` | text on dark, print background |
| Muted | `#8B93A1` | secondary text |
| Ember | `#FF7A2F` | the mark, primary action, one accent per screen |
| Ember Light | `#FFB27A` | hover, gradient top stop |
| Mint | `#3DDC97` | completed, connected |
| Amber | `#E6C36A` | paused, warning |
| Signal Red | `#FF5D5D` | failed only |

Ember on Ink is the signature pairing. Never place Ember text on Paper at body
size — it fails contrast; use Ink on Ember or Ember as a fill only.

## Type

System stacks only, because the app ships no fonts: `-apple-system,
BlinkMacSystemFont, "Segoe UI", Inter, system-ui, sans-serif`.

- Wordmark: lowercase `footorrent`, weight 650, tracking `-0.02em`. `foot` is
  Ember, `orrent` is Paper.
- Headings: 600, tracking `-0.01em`.
- Body: 400 at 13–15 px, line height 1.45.
- Numerals are tabular in the download table so sizes and speeds line up.

## Logo

Files in `assets/brand/`:

| File | Use |
| --- | --- |
| `footorrent-mark.svg` | 64 px tile: favicon, app icon, avatars |
| `footorrent-lockup.svg` | mark + wordmark, horizontal |
| `icon-1024.svg` | macOS / store icon source |

Clear space is half a toe radius on all sides. Minimum tile size is 16 px; below
that, drop the carved arrow and keep the footprint silhouette. Do not rotate,
recolor outside the palette, add a gradient to the mark, or place the lockup on
a photograph.

## Voice

Plain, specific, unimpressed with itself. Numbers over adjectives: "3.2 MB",
"1,546,822 bytes", not "blazing fast". The UI says `fetching`, `downloading`,
`completed`; it does not say "Awesome!" or "Oops!". Error messages name the
thing that failed and what to try next.

Writing the name: **Footorrent** in prose, `footorrent` in the wordmark, `ftc`
in code, paths and commands.

## UI tokens

The page uses the palette above directly: `--bg: #0b0c0f`, `--panel: #0f1115`,
`--accent: #ff7a2f`, `--ok: #3ddc84`. One Ember element per view — the Add
Download button — everything else is neutral so status colors stay meaningful.
