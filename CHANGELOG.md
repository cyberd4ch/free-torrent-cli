# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.0.0] - 2026-11-07

### Changed
- **Rewritten in Rust.** The engine lives in `crates/core`, the binary in
  `crates/cli` (`ftc`). Ported: bencode, metainfo and magnet parsing, HTTP + UDP
  trackers, the peer wire protocol, SHA-1 piece verification, multi-file storage,
  resume verification and tracker refill.
- Info hashes are computed from the original `info` dictionary bytes, not a
  re-encoded value, so unusual key ordering cannot change a torrent's identity.
- Torrent paths are validated; `..`, absolute paths and separators are rejected
  before any file is opened.
- Peer frames are capped at 2 MiB before allocation.

### Added
- 25 tests, including an end-to-end download from a fake local peer.
- `--peers` to bypass the tracker and `--max-peers` to tune concurrency.

### Removed
- The Node.js engine, Electron app, web UI and their tests. Magnet *downloads*
  (BEP 9/10), DHT (BEP 5), seeding and the front ends are staged; see the README
  parity table.

## [Unreleased]

### Added
- **Metadata exchange (BEP 9/10).** Magnet links now download: the client fetches the
  `info` dictionary from a peer, verifies it against the magnet's info hash, and builds
  the torrent from those exact bytes. Verified end to end against a fake peer.
- `Torrent::from_info` preserves the raw info dictionary, so the recomputed hash still
  matches the magnet.
- **DHT (BEP 5) peer discovery and announce.** A KRPC client over UDP: 160-bucket
  routing table, XOR-ordered `get_peers` hunts with token capture from every node
  that hands one, and `announce_peer` to the token-holding closest nodes after a
  download. Trackerless magnets resolve through it; `--no-dht` opts out.
- 34 tests, including a UDP router → holder chain exercising a full DHT hunt and a
  token-validated announce.
- **Seeding (`--seed <secs>`).** After the download completes, the engine opens a
  listener and serves exactly the pieces `verify_existing` passes: MSB-first
  Bitfield, unchoke on interest, 128 KiB request cap, one thread per peer, and
  a report of bytes and peers served. A partially-downloaded directory seeds
  only what verifies — the seeder lies about nothing.
- 2 more tests: a fake leecher round-trip (block served, impossible request
  drops the connection) and a wrong-info-hash rejection; 37 in total.

### Fixed
- `announce_peer` retries a datagram that goes unacknowledged inside its
  budget instead of counting a dropped UDP packet as a silent node; a
  deterministic test drops the first announce to pin the behaviour.

### Added
- **Magnet link support** — `node index.js "magnet:?xt=urn:btih:…"` now works with no
  `.torrent` file, everywhere the engine runs (CLI; web server too)
- **DHT peer discovery (BEP 5)** — iterative `get_peers` lookup ordered by XOR
  distance, `announce_peer` to the K closest nodes, plus a full DHT node
  (routing table, signed tokens, swarm maps) served from the same UDP socket
- **Metadata exchange (BEP 9/10)** — extension-protocol handshakes with the
  reserved-bit set, parallel `ut_metadata` piece fetching from up to 8 peers,
  SHA-1-verified info-dict assembly, tracker URLs carried over from the magnet
- **Headless web UI** — `node server.js [--port 9090] [--host 127.0.0.1] [--output dir]`:
  browser dashboard (add magnet/.torrent, live progress, speeds, peers, ETA),
  JSON REST API (`/api/add`, `/api/remove`, `/api/torrents`) and SSE live
  streaming (`/api/events`); runs with no display for NAS/VPS/RPi use
- **Shared download engine** (`src/engine.js`) — event-driven, cancellable
  per-torrent engine used by the web server; the same shape can drive other
  front ends
- Test workflow (`.github/workflows/test.yml`) running the full suite on every
  push and pull request across Node 18/20/22
- Test suite grew from 55 to 76 tests: magnet parsing incl. Base32, DHT
  builders/parsers, live 3-node DHT integration over localhost UDP, fake
  ut_metadata seeder end-to-end, and a full web-server API round trip

### Fixed
- `release.yaml` was sitting at the repository root, where GitHub Actions ignores it — moved to `.github/workflows/release.yml` so tag builds, Releases, and Pages deployment actually run
- Electron app no longer forces DevTools open in packaged (production) builds
- README: Quick Start (prebuilt app / GUI / web server / CLI), GUI + web feature
  lists, updated project tree, phased roadmap

## [2.0.0]

### Added
- Electron GUI with concurrent download dashboard, seeding state, and show-in-folder
- Full BitTorrent wire protocol, UDP + HTTP trackers, multi-file torrents, resume, and seeding
- From-scratch test suite (55 tests)
