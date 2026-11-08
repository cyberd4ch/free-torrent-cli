<div align="center">

# ftc

### A BitTorrent client in one ~3 MB binary.

**Rust · from scratch · no libtorrent · no Electron · no runtime to install**

[![Release](https://img.shields.io/github/v/release/cyberd4ch/free-torrent-cli?style=flat-square&color=orange&label=release)](https://github.com/cyberd4ch/free-torrent-cli/releases/latest)
[![Tests](https://img.shields.io/github/actions/workflow/status/cyberd4ch/free-torrent-cli/test.yml?style=flat-square&label=tests)](https://github.com/cyberd4ch/free-torrent-cli/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20windows-000000?style=flat-square)
![Binary](https://img.shields.io/badge/binary-~3%20MB-brightgreen?style=flat-square)

It speaks the BitTorrent wire protocol itself — handshake, bencode, HTTP and UDP
trackers, DHT, metadata exchange, SHA-1 piece verification, seeding. Nothing is
delegated to a C library, and nothing is downloaded at install time.

</div>

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/cyberd4ch/free-torrent-cli/main/install.sh | sh
```

Detects macOS or Linux and the CPU, downloads the matching archive, checks it
against `SHA256SUMS`, and puts `ftc` on your PATH — `/usr/local/bin` when that is
writable, `~/.local/bin` otherwise. No toolchain, no runtime, no sudo unless you
ask for it.

On macOS it also installs **a real app**: `~/Applications/ftc.app`, a WKWebView
window (87 KB of Swift, `gui/macos/main.swift`) that starts the server itself and
quits it when you close the window. Released as `ftc-macos-app.zip`. On Windows
the installer adds a Start Menu entry and on Linux a `ftc.desktop` file, both
running `ftc gui`. One installed binary, both interfaces: the command line when
you want it, a window when you don't.

```powershell
# Windows, in PowerShell
irm https://raw.githubusercontent.com/cyberd4ch/free-torrent-cli/main/install.ps1 | iex
```

### Or grab an archive

Archives, not bare binaries — and that is deliberate. An executable downloaded as
a plain file loses its permission bits, so macOS and Linux hand you something that
double-clicks into a text editor showing megabytes of binary.

| Platform | Archive | Size |
| :--- | :--- | ---: |
| **macOS** app (Apple silicon) | [`ftc-macos-app.zip`](https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/ftc-macos-app.zip) | 2.2 MB |
| **macOS** Apple silicon | [`ftc-macos-arm64.tar.gz`](https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/ftc-macos-arm64.tar.gz) | 1.3 MB |
| **macOS** Intel | [`ftc-macos-x86_64.tar.gz`](https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/ftc-macos-x86_64.tar.gz) | 1.4 MB |
| **Linux** x86_64 | [`ftc-linux-x86_64.tar.gz`](https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/ftc-linux-x86_64.tar.gz) | 1.4 MB |
| **Windows** x86_64 | [`ftc-windows-x86_64.zip`](https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/ftc-windows-x86_64.zip) | 1.2 MB |

```bash
tar xzf ftc-macos-arm64.tar.gz      # ftc arrives as mode 0755
./ftc --version
```

```powershell
Expand-Archive ftc-windows-x86_64.zip -DestinationPath .
.\ftc.exe --version
```

Every release also ships [`SHA256SUMS`](https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/SHA256SUMS).
Verify before you run it:

```bash
curl -LO https://github.com/cyberd4ch/free-torrent-cli/releases/latest/download/SHA256SUMS
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

## Web UI

```bash
ftc gui                          # web ui, opened in your browser
ftc serve                        # http://127.0.0.1:8080, terminal only
ftc serve --port 9000 -o ~/Downloads
ftc serve --limit 2M             # default cap for every job added from the page
```

The same engine behind a local page: paste a magnet or a `.torrent` path, watch
progress, speed, peers and ETA, cancel a run. The HTML and the HTTP server are
compiled into the binary — no extra dependency, no second process — and it binds
loopback only unless you ask for `--bind`.

Underneath it is a small JSON API, form-encoded on the way in:

| Endpoint | |
| :--- | :--- |
| `GET /api/state` | every job with its live counters |
| `POST /api/add` | `target=<magnet or path>&output=<dir>&limit=<rate>` |
| `POST /api/pause` | `id=<job id>` |
| `POST /api/resume` | `id=<job id>` |
| `POST /api/cancel` | `id=<job id>` |

Jobs report the engine's own `Arc<Progress>` atomics, so the page and the
progress bar in your terminal can never disagree. Pause parks the workers without
touching verified progress; resume continues from the same queue.

`ftc gui` (and `ftc serve --open`) opens the page for you and defaults downloads
to `~/Downloads`, because a double-clicked launcher has no useful working
directory.

A `limit` (the box next to the output field) caps that one job; `--limit` at
startup sets the default. The cap is a single shared token bucket in the engine,
so `2M` means two mebibytes per second across all peers — not per connection —
and a rate it cannot parse is rejected rather than silently ignored.

## What you get for 3 MB

| | |
| :--- | :--- |
| **No runtime.** Static binary. No Node, no Python, no JVM, no Electron download on first launch. | **No libtorrent.** Bencode, trackers, wire protocol, DHT and seeding are this repo's code. |
| **Resumable.** Existing pieces are re-verified on disk, so a restart continues instead of redownloading. | **Honest seeding.** The Bitfield is built from verified pieces only — it never claims a piece it can't prove. |
| **Multi-file torrents.** Crosses file boundaries without stitching buffers. | **Safe paths.** `..`, absolute paths and separators are rejected before any file is opened. |
| **A real speed cap.** `--limit 2M` is one shared token bucket, so the number is the aggregate across peers — not a per-connection trickle. | **Pause and resume.** Workers park between blocks; verified pieces are never downloaded twice. |

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
| Pause / resume / cancel from the page, engine-side | ✅ |
| Download rate limit (`--limit`, per-job `limit`) | ✅ |

## How it's built

```
crates/core     the engine, as a library
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
`percent-encoding`). No async runtime: a worker thread per peer.

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

Rust 1.75+ (an MSRV-pinned `rust-toolchain.toml` is included):

```bash
git clone https://github.com/cyberd4ch/free-torrent-cli
cd free-torrent-cli
cargo build --release        # target/release/ftc
```

## Test

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

51 tests — 39 in the engine, 12 in the CLI. The suite is end-to-end where it
matters: a fake peer serves a real torrent and
the session downloads, verifies and writes it; a fake router → holder chain over
UDP exercises a full DHT hunt and a token-validated announce; a fake leecher
pulls one block from the seeder and is dropped for asking beyond the torrent.

## Roadmap

1. A desktop front end on top of the local JSON API.

## License

MIT — see [LICENSE](LICENSE).
