<div align="center">

# 🌀 free-torrent-cli

**A lightweight, from-scratch BitTorrent client built with Node.js**

![Node.js](https://img.shields.io/badge/Node.js-v18%2B-339933?style=flat-square&logo=node.js&logoColor=white)
![License](https://img.shields.io/badge/License-MIT-blue?style=flat-square)
![Tests](https://img.shields.io/badge/Tests-55%20passing-brightgreen?style=flat-square)
![Status](https://img.shields.io/badge/Status-Active-success?style=flat-square)

A fully functional BitTorrent client implemented from the ground up in Node.js — no libtorrent, no shortcuts. Speaks the BitTorrent wire protocol directly, handles UDP and HTTP trackers, verifies every piece with SHA-1, and seeds back to the swarm while downloading.

</div>

---

## Table of Contents

- [Features](#features)
- [Architecture](#architecture)
- [Installation](#installation)
- [Usage](#usage)
- [How It Works](#how-it-works)
- [Project Structure](#project-structure)
- [Running Tests](#running-tests)
- [Planned Features](#planned-features)
- [Known Limitations](#known-limitations)

---

## Features

### ✅ Core Protocol
- **BitTorrent wire protocol** — full handshake, choke/unchoke, interested/not-interested, bitfield, have, request, piece, cancel messages
- **UDP tracker protocol** — connect/announce flow per BEP 15
- **HTTP tracker protocol** — compact peer format, binary info_hash and peer_id encoding
- **Multi-tracker fallback** — tries all trackers in priority order (UDP first, then HTTP/HTTPS), automatically falls back to the next if one times out

### ✅ Downloading
- **Piece assembly** — accumulates 16 KiB blocks into full pieces before writing
- **SHA-1 piece verification** — every piece is hashed and compared against torrent metadata before being written to disk. Corrupt pieces are discarded and re-requested automatically
- **Resume support** — on restart, existing files are scanned and each piece is SHA-1 verified. Only clean, verified pieces are skipped — corrupt or partial pieces are re-downloaded
- **Auto re-announce** — when peers run out mid-download, the client re-announces to the tracker after 10 seconds and continues with a fresh peer list
- **Connection manager** — maintains up to 10 simultaneous peer connections, cycling through the full peer list as connections close

### ✅ Multi-file Torrents
- **Full multi-file support** — correctly maps pieces across file boundaries within a torrent's logical byte stream
- **Automatic directory creation** — output directory structure mirrors the torrent's file tree
- **Single-file torrents** — treated as a degenerate case of multi-file; no special handling needed at the call site

### ✅ Seeding
- **Concurrent seeding** — begins seeding immediately when downloading starts, not after completion
- **Bitfield advertisement** — sends peers a bitfield on connect showing exactly which pieces are available
- **Have announcements** — broadcasts `have` messages to all connected seed peers whenever a new piece is verified
- **Upload slot management** — unchokes up to 4 peers simultaneously (tit-for-tat foundation)
- **Request validation** — validates piece index, begin offset, and block length before reading from disk; rejects oversized requests (> 32 KiB)

### ✅ CLI & UX
- **Live progress bar** — shows percentage, downloaded bytes, download speed, upload speed, ETA, active peer counts, verified piece count, and failed piece count
- **Coloured output** — header, status messages, and errors are colour-coded via chalk
- **Torrent info summary** — displays name, type (single/multi-file), size, piece count, piece size, and output path before starting
- **File listing** — multi-file torrents show each file and its size in the header
- **Optional output directory** — specify where files are saved as a second argument
- **Usage message** — running with no arguments prints usage and examples

---

## Architecture

```
free-torrent-cli/
├── index.js              Entry point — parses args, opens torrent, starts download
└── src/
    ├── TorrentParser.js  Bencode decoding, info hash, size calculation
    ├── tracker.js        UDP + HTTP tracker protocols, multi-tracker fallback
    ├── download.js       Peer connections, piece assembly, verification, progress UI
    ├── FileManager.js    Multi-file layout, cross-boundary piece writes/reads
    ├── Seeder.js         TCP server, incoming peer handling, piece serving
    ├── Message.js        BitTorrent message builders and parser
    ├── Pieces.js         Piece state tracking (needed / requested / received)
    └── util.js           Peer ID generation
```

**Data flow:**

```
.torrent file
     │
     ▼
TorrentParser  ──►  tracker.js  ──►  peer list
                                          │
                                          ▼
                                    download.js
                                    │         │
                                    ▼         ▼
                              FileManager   Seeder
                              (writes)    (serves)
                                    │
                                    ▼
                              Pieces (SHA-1 verified)
```

---

## Installation

**Prerequisites:** Node.js v18 or higher

```bash
# Clone the repository
git clone https://github.com/cyberd4ch/free-torrent-cli.git
cd free-torrent-cli

# Install dependencies
npm install
```

**Dependencies:**

| Package | Purpose |
|---|---|
| `bencode` | Torrent file decoding |
| `url-parse` | UDP tracker URL parsing |
| `chalk` | Terminal colours |
| `cli-progress` | Live progress bar |

---

## Usage

```bash
# Basic usage — saves to current directory
node index.js <torrent-file>

# Specify output directory
node index.js <torrent-file> <output-dir>
```

**Examples:**

```bash
# Download a single-file torrent
node index.js kali-linux-2026.1-installer-arm64.iso.torrent

# Download to ~/Downloads
node index.js kali-linux-2026.1-installer-arm64.iso.torrent ~/Downloads

# Download a multi-file torrent
node index.js ubuntu-22.04.torrent ~/Downloads
```

**Example output:**

```
  free-torrent-cli
  ─────────────────────────────────────────────
  Name   : kali-linux-2026.1-installer-arm64.iso
  Type   : Single-file
  Size   : 3967.46 MB
  Pieces : 15135 × 256 KiB
  Output : /Users/you/Downloads
  ─────────────────────────────────────────────

  Checking for existing progress... resumed 412/15135 pieces verified ✓

  ████░░░░░░░░░░░░░░░░░░ 14.2%
  ↓ 1.4 MB/s  ↑ 230 KB/s  |  562.3 MB/3967.46 MB  |  ETA 42m 10s  |  Peers ↓8 ↑2  |  ✓ 412  ✗ 0

  ✓ Download complete!
  Elapsed  : 48m 22s
  Downloaded: 3967.46 MB
  Uploaded  : 142.3 MB
  Verified  : 15135 pieces
  Bad pieces: 3 (re-requested)

  Switching to seed mode... (Ctrl+C to stop)
```

---

## How It Works

### Tracker Communication
The client collects all tracker URLs from the torrent's `announce` and `announce-list` fields. UDP trackers are tried first (faster, lower overhead), followed by HTTP/HTTPS trackers. Each tracker gets a 5-second timeout before the next is tried. After a successful announce, the client receives a compact peer list and begins connecting.

### Peer Connections
The client maintains up to 10 simultaneous outbound connections. For each peer it:
1. Sends a handshake with the info hash and peer ID
2. Sends `interested`
3. Waits for `unchoke`
4. Reads the peer's `bitfield` or `have` messages to know which pieces it has
5. Requests pieces in 16 KiB blocks

### Piece Verification
Blocks from a peer are accumulated in memory into a full piece buffer. When the last block of a piece arrives, the entire piece is SHA-1 hashed and compared against the hash stored in the torrent metadata. If the hash matches, the piece is written to disk via `FileManager`. If not, it is silently discarded and re-queued.

### Multi-file Layout
BitTorrent treats a multi-file torrent as a single contiguous byte stream. `FileManager` maps this stream to actual files on disk. When writing or reading a piece that spans two files, it splits the operation correctly across both file descriptors.

### Seeding
`Seeder` runs a TCP server on port 6881 in parallel with the download. When a peer connects it validates the handshake info hash, sends back our handshake and bitfield, then waits for `interested`. Interested peers are unchoked (up to 4 at a time) and their `request` messages are served by reading the requested piece from disk and sending a `piece` message back.

---

## Project Structure

```
free-torrent-cli/
├── index.js
├── package.json
├── README.md
├── LICENSE
├── src/
│   ├── TorrentParser.js
│   ├── tracker.js
│   ├── download.js
│   ├── FileManager.js
│   ├── Seeder.js
│   ├── Message.js
│   ├── Pieces.js
│   └── util.js
└── tests/
    └── run.js
```

---

## Running Tests

```bash
npm test
```

The test suite covers all modules with 55 unit tests and requires no network access or real torrent files — all I/O is performed against temporary files and in-memory mocks.

```
── util.js ──────────────────────────────────  3 tests
── TorrentParser.js ─────────────────────────  5 tests
── tracker.js ───────────────────────────────  11 tests
── Message.js ───────────────────────────────  16 tests
── Pieces.js ────────────────────────────────  7 tests
── FileManager.js ───────────────────────────  7 tests
── Seeder.js ────────────────────────────────  5 tests
                                    ──────────────────
                                    55 passed, 0 failed
```

---

## Planned Features

### 🔵 Magnet Link Support
Magnet links are the dominant way torrents are shared today — no `.torrent` file needed. Implementing this requires two components:

- **DHT (Distributed Hash Table)** — BEP 5. A decentralised peer discovery mechanism that finds peers using only an info hash, without relying on a tracker. Involves implementing a Kademlia-based routing table, `find_node` and `get_peers` UDP queries, and maintaining a bucket of known DHT nodes.
- **Metadata extension** — BEP 9 + BEP 10. Once peers are found via DHT, the torrent metadata (the `info` dictionary) is fetched directly from peers using the extension protocol. This replaces the `.torrent` file entirely.

```bash
# Target usage
node index.js "magnet:?xt=urn:btih:HASH&dn=name&tr=..."
```

### 🔵 Robust Choking Algorithm
The current seeder uses a simple fixed-slot unchoke (4 peers). A full tit-for-tat implementation per BEP 3 would include:

- **Download-rate based unchoking** — rank leeching peers by how fast they are uploading to us; unchoke the top N
- **Optimistic unchoke** — every 30 seconds, randomly unchoke one additional peer regardless of upload rate, giving new peers a chance to prove themselves
- **Anti-snubbing** — if a peer hasn't sent us a piece in 60 seconds despite being unchoked, consider them snubbed and replace them
- **Seeding mode** — when fully seeded, switch ranking criterion from download rate to upload rate (reward peers who help the swarm)

This significantly improves download speed in practice by incentivising peers to upload to you in return.

---

## Known Limitations

- **No magnet link support** — requires a `.torrent` file (planned, see above)
- **UDP often blocked** — many networks block outbound UDP; the client falls back to HTTP trackers automatically
- **No DHT** — peer discovery relies entirely on trackers
- **Basic choking** — fixed 4-slot unchoke without rate-based ranking (planned, see above)
- **No encryption** — BitTorrent protocol encryption (MSE/PE) is not implemented; some ISPs throttle unencrypted BitTorrent traffic
- **Single piece pipeline** — requests one piece per peer at a time; pipelining multiple requests would improve throughput

---

## License

MIT — see [LICENSE](LICENSE) for details.

---

<div align="center">
Built from scratch with Node.js · No libtorrent · No shortcuts
</div>