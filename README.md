<div align="center">

# Footorrent

### Small footprint. Big downloads.

**The free, open-source BitTorrent client that fits in one ~3 MB binary.**
`ftc` is the command; **Footorrent** is the product. The brand book is in [`BRAND.md`](BRAND.md).

**Rust · from scratch · no libtorrent · no Electron · no runtime to install**
**No ads · no bundles · no telemetry · no paid tier**

[![Release](https://img.shields.io/github/v/release/cyberd4ch/footorrent?style=flat-square&color=orange&label=release)](https://github.com/cyberd4ch/footorrent/releases/latest)
[![Tests](https://img.shields.io/github/actions/workflow/status/cyberd4ch/footorrent/test.yml?style=flat-square&label=tests)](https://github.com/cyberd4ch/footorrent/actions/workflows/test.yml)
[![License](https://img.shields.io/github/license/cyberd4ch/footorrent?style=flat-square&label=license)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20windows-000000?style=flat-square)
![Binary](https://img.shields.io/badge/binary-~3%20MB-brightgreen?style=flat-square)

It speaks the BitTorrent wire protocol itself — handshake, bencode, HTTP and UDP
trackers, DHT, metadata exchange, SHA-1 piece verification, seeding. Nothing is
delegated to a C library, and nothing is downloaded at install time.

</div>

---

## Why Footorrent

The big torrent clients became the reason you need an alternative. µTorrent and
BitTorrent grew into ad platforms — banner slots, bundled offers, upsells in the
installer. qBittorrent and Deluge cleaned up the ethics but ship tens of
megabytes of runtime for what is, underneath, one protocol. And closed-source
clients ask you to trust a black box with your network activity.

Footorrent is the same promise the small clients of the 2000s made, kept
honestly:

- **Free as in price and as in freedom.** [GPL-3.0-or-later](LICENSE) — the
  same copyleft the rest of the torrent ecosystem relies on. No ads, no
  bundles, no pro tier, no telemetry, and nobody — including this repo — can
  ever take it closed. Any fork that ships Footorrent's code has to ship its
  sources too.
- **One ~3 MB binary.** A static build you can audit in an afternoon. There is
  no libtorrent underneath, no Electron shell around it, no runtime to
  download on first launch. Six crate dependencies in total; every protocol
  byte is code in this repo.
- **Works through a dead tracker list.** Magnets resolve peers via the DHT
  alone, pull the real metadata off them and verify it against the info hash
  before writing a single byte. And when a download stalls, `ftc swarm` tells
  you the truth — dead swarm or broken client.
- **A real interface in both directions.** A full CLI that renders progress in
  place, and a local web UI that behaves like a download manager — same
  engine, one binary.

### Use it for things you're allowed to

Footorrent is a tool. Linux ISOs, public-domain media, your own backups and
distributed datasets are what the BitTorrent protocol was built for. What you
download is up to you and your law; the GPL protects this program's freedom,
not anyone's infringement. Nothing in the client phones home, ever.

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/cyberd4ch/footorrent/main/install.sh | sh
```

Detects macOS or Linux and the CPU, downloads the matching archive, checks it
against `SHA256SUMS`, and puts `ftc` on your PATH — `/usr/local/bin` when that is
writable, `~/.local/bin` otherwise. It also installs **`footorrent`** beside it
as an alias for the same binary; on Windows the installer writes
`footorrent.cmd`. No toolchain, no runtime, no sudo unless you ask for it.

On macOS it also installs **a real app**: `~/Applications/Footorrent.app`, a WKWebView
window (87 KB of Swift, `gui/macos/main.swift`) that starts the server itself and
quits it when you close the window. Released as `ftc-macos-app.zip`. On Windows
the installer adds a Start Menu entry and on Linux a `ftc.desktop` file, both
running `ftc gui`. One installed binary, both interfaces: the command line when
you want it, a window when you don't.

```powershell
# Windows, in PowerShell
irm https://raw.githubusercontent.com/cyberd4ch/footorrent/main/install.ps1 | iex
```

### Or grab an archive

Archives, not bare binaries — and that is deliberate. An executable downloaded as
a plain file loses its permission bits, so macOS and Linux hand you something that
double-clicks into a text editor showing megabytes of binary.

| Platform | Archive | Size |
| :--- | :--- | ---: |
| **macOS** app (Apple silicon) | [`ftc-macos-app.zip`](https://github.com/cyberd4ch/footorrent/releases/latest/download/ftc-macos-app.zip) | 1.4 MB |
| **macOS** Apple silicon | [`ftc-macos-arm64.tar.gz`](https://github.com/cyberd4ch/footorrent/releases/latest/download/ftc-macos-arm64.tar.gz) | 1.4 MB |
| **macOS** Intel | [`ftc-macos-x86_64.tar.gz`](https://github.com/cyberd4ch/footorrent/releases/latest/download/ftc-macos-x86_64.tar.gz) | 1.5 MB |
| **Linux** x86_64 | [`ftc-linux-x86_64.tar.gz`](https://github.com/cyberd4ch/footorrent/releases/latest/download/ftc-linux-x86_64.tar.gz) | 1.5 MB |
| **Windows** x86_64 | [`ftc-windows-x86_64.zip`](https://github.com/cyberd4ch/footorrent/releases/latest/download/ftc-windows-x86_64.zip) | 1.3 MB |

```bash
tar xzf ftc-macos-arm64.tar.gz      # ftc arrives as mode 0755
./ftc --version
```

```powershell
Expand-Archive ftc-windows-x86_64.zip -DestinationPath .
.\ftc.exe --version
```

Every release also ships [`SHA256SUMS`](https://github.com/cyberd4ch/footorrent/releases/latest/download/SHA256SUMS).
Verify before you run it:

```bash
curl -LO https://github.com/cyberd4ch/footorrent/releases/latest/download/SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing      # linux: sha256sum -c
```

## Use

```bash
ftc ./some.torrent -o ./downloads
ftc "magnet:?xt=urn:btih:..." -o ./downloads
ftc ./some.torrent -o ./downloads --seed 300     # keep serving after it finishes
```

```
USAGE:
    ftc <file.torrent | magnet:?xt=urn:btih:...> [options]

OPTIONS:
    -o, --output <dir>    where files are written (default: current directory)
        --peers <list>    comma-separated host:port peers, skipping the tracker
        --max-peers <n>   simultaneous peer connections (default: 6)
        --no-dht          skip DHT peer discovery (trackers only)
        --seed <secs>     after finishing, serve pieces to peers for <secs>s
        --limit <rate>    cap the download rate, e.g. 500k or 2M (default: unlimited)
```

Progress renders in place, no log spam:

```
[██████████████░░░░░░░░░░░░░░░░]  47%  9/19  1.8 MiB/s  4 peers
```

A magnet with no tracker still works: it hunts peers through the DHT, fetches
the `info` dictionary from them (BEP 9/10), and verifies it against the info
hash before writing a single byte.

Adding a magnet from the web UI is deliberate on purpose: step one contacts the
swarm and confirms what it actually is — real name, exact byte size, piece
count and file list — step two asks where it should go, and only then does
anything touch disk.

### Is the swarm dead, or is the client stuck?

```bash
ftc swarm "magnet:?xt=urn:btih:..."
```

Announces to every tracker in the link — falling back to public trackers when
that list is dead — asks the DHT, dedupes the peers and TCP-probes them, then
reports how many trackers answered and how many peers actually accept a
connection. A magnet with 20 known peers and 2 reachable is a dead swarm, not a
broken download.

## Web UI

```bash
ftc gui                          # web ui, opened in your browser
ftc serve                        # http://127.0.0.1:8080, terminal only
ftc serve --port 9000 -o ~/Downloads
ftc serve --limit 2M             # default cap for every job added from the page
```

The same engine behind a local download manager: categories with live counts, a
sortable table (size, progress, speed, time left, peers, status, age), multi-select
with Start/Pause/Stop/Retry/Remove, an add sheet that confirms a magnet before it
can write anything, failed jobs carrying a Retry button and a **Copy infohash**
action, and per-peer rates in the details pane. Drop a `.torrent` file
anywhere on the page or a magnet link, and it queues; Tools clears finished jobs,
exports the list as JSON and copies every path, the sidebar keeps a 60-second speed
graph, and Preferences can send a desktop notification when a download finishes or
fails, and can set the default folder and cap. The HTML, CSS and JS are compiled
into the binary — no extra dependency, no second process — and it binds loopback
only unless you ask for `--bind`.

Underneath it is a small JSON API, form-encoded on the way in:

| Endpoint | |
| :--- | :--- |
| `GET /api/state` | every job with live counters, session totals and the current settings |
| `GET`/`POST /api/settings` | default folder (`output=`) and cap (`limit=`, `none` clears it) |
| `POST /api/add` | `target=<magnet or path>&output=<dir>&limit=<rate>&paused=1` |
| `POST /api/resolve` | two-step magnets: contact the swarm, return name, size, piece count, file list and an add token |
| `POST /api/start` | `token=<resolve token>&output=<dir>&limit=<rate>` — begin the confirmed download |
| `POST /api/add-torrent` | a dropped file: `name=<file>&data=<base64>&output=<dir>&limit=<rate>` |
| `POST /api/pause` / `resume` | `id=<job>`, `ids=1,2,3` or `all=1` |
| `POST /api/cancel` | `id=<job>`, `ids=1,2,3` or `all=1` |
| `POST /api/remove` | drop finished jobs from the list: `id=`, `ids=` or `finished=1` |
| `POST /api/retry` | retry a stopped or failed job, or resume a paused one: `id=<job>` |
| `POST /api/reveal` | show a job's file or folder in the file manager: `id=<job>` |

Jobs report the engine's own `Arc<Progress>` atomics, so the page and the
progress bar in your terminal can never disagree. Pause parks the workers without
touching verified progress; resume continues from the same queue. Pause and Stop
land between 16 KiB blocks, not at the end of a piece, so the UI reads the truth
in real time.

`ftc gui` (and `ftc serve --open`) opens the page for you and defaults downloads
to `~/Downloads`, because a double-clicked launcher has no useful working
directory.

The speed limit in the add sheet caps that one job, and Preferences (or `--limit`
at startup) sets the default for the ones added afterwards. The cap is a single
shared token bucket in the engine, so `2M` means two mebibytes per second across
all peers — not per connection — and a rate it cannot parse is rejected rather
than silently ignored.

## What you get for 3 MB

| | |
| :--- | :--- |
| **No runtime.** Static binary. No Node, no Python, no JVM, no Electron download on first launch. | **No libtorrent.** Bencode, trackers, wire protocol, DHT and seeding are this repo's code. |
| **No ads, no bundles, no telemetry.** The protocol stack talks to trackers, DHT peers and nothing else. | **Honest seeding.** The Bitfield is built from verified pieces only — it never claims a piece it can't prove. |
| **Resumable.** Existing pieces are re-verified on disk, so a restart continues instead of redownloading. | **Multi-file torrents.** Crosses file boundaries without stitching buffers. |
| **A real speed cap.** `--limit 2M` is one shared token bucket, so the number is the aggregate across peers — not a per-connection trickle. | **Pause and resume.** Workers park between blocks; verified pieces are never downloaded twice. |
| **Safe paths.** `..`, absolute paths and separators are rejected before any file is opened. | **Capped frames.** Peer messages cannot exhaust memory — sound from the first packet. |

## Protocol coverage

| Area | |
| :--- | :--- |
| Bencode codec, exact `info` byte-span for hashing | ✅ |
| `.torrent` parsing — single and multi-file | ✅ |
| Magnet URIs — hex and base32 info hashes | ✅ |
| Metadata exchange (BEP 9/10) — magnet-only downloads | ✅ |
| HTTP tracker announces — compact and dictionary peers | ✅ |
| UDP tracker announces (BEP 15) | ✅ |
| Peer wire protocol — handshake, bitfield, choke, request/piece | ✅ |
| SHA-1 piece verification, corrupt pieces re-queued | ✅ |
| Resume from disk, multi-file storage mapping | ✅ |
| Tracker re-announce and peer refill | ✅ |
| DHT (BEP 5) — `get_peers` hunting and announce | ✅ |
| Seeding (`--seed`) | ✅ |
| Web UI + local JSON API (`ftc serve`) | ✅ |
| Two-step magnet adds (resolve + confirm + start) | ✅ |
| Pause / resume / stop / retry from the page, engine-side | ✅ |
| Swarm diagnostics (`ftc swarm`) — trackers, DHT, TCP reachability | ✅ |
| Download rate limit (`--limit`, per-job `limit`) | ✅ |

## How it's built

```
crates/core     the engine, as a library ftc-core
  bencode.rs    encoder/decoder; records raw info-dict bytes for hashing
  torrent.rs    metainfo, piece table, file layout, sanitised paths
  magnet.rs     magnet URIs, hex and base32 info hashes
  tracker.rs    HTTP + UDP announces, compact peer decoding
  dht.rs        BEP 5: KRPC, routing table, announce
  peer.rs       wire framing and message codec
  seed.rs       serve verified pieces to inbound peers
  storage.rs    logical stream → files, piece writes, resume verification
  session.rs    peer workers, retrying pieces, progress accounting, rate limit
crates/cli      the `ftc` binary — argument parsing, progress rendering
  http.rs       a blocking HTTP/1.1 server, no dependency
  serve.rs      `ftc serve`: job registry, JSON API, cancellation
  ui.html       the single-page UI, embedded with include_str!
```

Six dependencies in total (`sha1`, `hex`, `rand`, `ureq`, `base32`,
`percent-encoding`). No async runtime: a worker thread per peer. The crates
keep their `free-torrent-cli` / `ftc-core` names so nothing anyone typed
before stops working — `BRAND.md` is the contract for which names are which.

Decisions worth knowing:

- **The info hash is computed over the original `info` bytes**, not a re-encoded
  dictionary, so unusual key ordering can't change a torrent's identity.
- **Peer frames are capped at 2 MiB** before allocation; a hostile length header
  can't exhaust memory.
- **A bad piece goes back on the queue**, a misbehaving peer is dropped, and the
  run fails only after four refill attempts.
- **The seeder lies about nothing**: a request outside the verified set
  disconnects the peer rather than guessing.

## Build from source

Rust stable (the included `rust-toolchain.toml` pins the stable toolchain with
`clippy` and `rustfmt`; edition 2021):

```bash
git clone https://github.com/cyberd4ch/footorrent
cd footorrent
cargo build --release        # target/release/ftc
```

## Test

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

63 tests — 41 in the engine, 22 in the CLI. The suite is end-to-end where it
matters: a fake peer serves a real torrent and
the session downloads, verifies and writes it; a fake router → holder chain over
UDP exercises a full DHT hunt and a token-validated announce; a fake leecher
pulls one block from the seeder and is dropped for asking beyond the torrent.

## Contributing

Issues and pull requests are welcome. A few ground rules that keep the project
small and yours to trust:

- `cargo test --all && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`
  must pass — CI runs the same gate.
- New transports or features should fail soft: a dead tracker or a hostile peer
  is data, not a crash.
- No new runtime dependencies unless the protocol truly needs them; the six
  existing ones are a design decision, not an accident.
- Contributed code arrives under GPL-3.0-or-later — that is how the client
  stays free for the next person too.

## Roadmap

1. A native Windows front end on the same local JSON API (the macOS app already
   runs on it).

## License

Footorrent is free software under the
**[GNU GPL v3.0-or-later](LICENSE)** — copyright © 2025–2026 dach-a.

That means:

- **You can use it for anything, forever, at $0.**
- **You can read, modify and redistribute it** — and your forks must carry the
  same GPL grant and release their source, which is exactly how this client
  stays auditable and ad-free instead of becoming another installer with
  offers in it.
- **No hidden terms.** There is no EULA, seat limit or "premium" waiting
  behind the first-run dialog; the ban on bundling is structural — a fork with
  ads still owes you its code.

The torrent-client ecosystem is one of open licensing's success stories
(Transmission, qBittorrent, Deluge, rTorrent all ship under GPL licenses).
Footorrent joins that lineage rather than leaving it.
