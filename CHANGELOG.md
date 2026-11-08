# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.3.0] - 2026-11-08

### Added
- **The page is a download manager now, not a form.** Categories with live counts
  (all, downloading, paused, completed, failed), a sortable table with size,
  progress, speed, time left, peers, status and age, multi-select with Start,
  Pause, Stop, Retry and Remove, search, a details pane (path, pieces, peers,
  average speed, elapsed, limit, timestamps, error), a right-click menu, a status
  bar with aggregate speed, and shortcuts (`⌘N` add, `⌘F` search, `Space`
  pause/resume, `Delete` stop, `⇧Delete` remove, `⌘A` select the view).
- **Add sheet** that queues every pasted link at once, with a save folder, a per-job
  cap and a "start paused" option; **Preferences** for the default folder and cap,
  applied from then on and reported by the API.
- **Batch and lifecycle endpoints.** `id=`, `ids=1,2,3` or `all=1` for pause,
  resume and cancel; `remove` drops finished jobs (`finished=1` clears them all);
  `retry` queues a failed or stopped job again; `reveal` opens the file or folder
  in the system file manager; `GET`/`POST /api/settings` reads and writes defaults.
- `/api/state` grew `version`, `settings`, `totals` and per-job `path`, `percent`,
  `added`, `finished` and verified/total pieces, so the table needs no guessing.

### Fixed
- **The window no longer orphans its server when it is killed.** AppKit runs
  `applicationWillTerminate` only for its own quit path, so a logout, a `kill` or a
  Ctrl-C on the app left `ftc serve` running on a loopback port with a full job
  queue and no window. The app now traps `SIGTERM`/`SIGINT`/`SIGHUP` through dispatch
  sources, terminates the child, waits, and escalates to `SIGKILL` if it is still
  alive — the same shutdown the Quit menu takes.

### Verified
- With the window running, `SIGTERM`, `SIGINT` and `SIGHUP` each stop the child
  (no listener left, `pgrep -P` empty) while the page served `200` before the signal.
- 17 CLI tests cover the new API against the real server (settings round-trip, batch
  pause/resume, remove-only-finished, retry, reveal); the page script parses with
  `node --check`. The bundle was rebuilt, relaunched and served the new page.

## [3.2.1] - 2026-11-08

### Added
- **A real macOS app.** `gui/macos/main.swift` is a small AppKit + WKWebView window:
  it starts `ftc serve` on a free loopback port, reads the URL the server prints,
  shows that page, and terminates the child on quit. CI compiles it into `ftc.app`
  and attaches `ftc-macos-app.zip` to the release; `install.sh` unpacks the bundle
  into `~/Applications` (and still writes a browser launcher for older releases).
  No Rust dependency was added — the window is 87 KB of Swift around the same binary.
- `FTC_BASE_URL` on `install.sh` to test the installer against a local directory.

### Verified
- The installed bundle launches, spawns its own server, serves the page (`200`) and
  leaves no child behind after an AppKit quit. A signal quit did not — see 3.2.2.

## [3.2.0] - 2026-11-08

### Added
- **Download rate limit.** `--limit 500k` caps a CLI download, `ftc serve --limit 2M`
  sets the default for jobs added from the page, and the UI takes a per-job `limit`
  (`limit=` on `POST /api/add`). The bucket lives in the engine and is shared by every
  peer worker, so the number is the aggregate rate rather than a per-connection cap.
  A rate that cannot be parsed is a `400`, not a silently unlimited job.
- `Throttle` in `ftc-core::session`: `bytes_per_second`, reserve-before-request
  pacing, and a test that proves 40 KB at 80 KB/s takes at least 300 ms.

### Fixed
- **The DHT hunt could find only one token holder**, which made
  `hunts_peers_through_the_router_chain_then_announces` fail roughly one run in
  four. `peer_hunt` stopped reading a round's replies as soon as one peer came
  back, so `get_peers` responses still in flight never recorded their tokens and
  `announce` reached fewer nodes than it knew about. The search now drains the
  round — the outer loop still stops once peers are found — and the test reports
  the token-holder count when it fails. 40/40 isolated runs, 25/25 for the retry
  test.
- **A reply could be discarded as unknown.** `ask` registered its transaction
  after the datagram was sent; on loopback a fast reply could arrive in between
  and be dropped as stale. Registration now happens before the send, and a
  failed send removes the entry again.

## [3.1.0] - 2026-11-08

### Added
- **Web UI (`ftc serve`).** A local single-page interface on `http://127.0.0.1:8080`:
  paste a magnet or a `.torrent` path, watch progress, speed, peers and ETA, and
  cancel a job. Served by a small hand-written HTTP/1.1 server in `crates/cli/src/http.rs`,
  so the dependency count stays at six and the binary stays self-contained.
- **JSON API** behind the UI: `GET /api/state`, `POST /api/add`, `POST /api/cancel`
  (form-encoded). Jobs report the engine's own `Progress` counters — the UI polls the
  same atomics the CLI renders from, so the two can never disagree.
- **Pause and resume.** `Session::pause_handle()` parks workers without dropping
  verified progress: no new piece is requested while the flag is set, and the
  download continues from the same queue when it clears. The UI grows
  Pause/Resume buttons and `POST /api/pause` / `POST /api/resume` back them.
- **Cancellation in the engine.** `Session::stop_handle()` returns a flag that is
  checked between peer refills and after each worker round; the run ends with
  `Error::Cancelled`.
- **`ftc gui`** starts the server on a free port, opens it in the default browser
  and defaults downloads to `~/Downloads` — the double-click path, as opposed to
  the terminal one. `ftc serve --open` does the same on a chosen port.
- **Desktop launchers.** `install.sh` writes `~/Applications/ftc.app` on macOS and
  `~/.local/share/applications/ftc.desktop` on Linux; `install.ps1` adds a Start
  Menu shortcut. One installed binary, both interfaces.

### Fixed
- The web UI serves a real SVG favicon instead of answering `204`, so browsers no
  longer log a failed `/favicon.ico` request.
- A download directory that does not exist yet is created instead of failing the
  job.

## [3.0.0] - 2026-11-08

First release of the Rust client. Prebuilt `ftc` binaries for Linux x86_64,
macOS (arm64 + x86_64) and Windows x86_64, plus `SHA256SUMS`, are attached to
the GitHub release.

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
- **Metadata exchange (BEP 9/10).** Magnet links now download: the client fetches the
  `info` dictionary from a peer, verifies it against the magnet's info hash, and builds
  the torrent from those exact bytes. Verified end to end against a fake peer.
- `Torrent::from_info` preserves the raw info dictionary, so the recomputed hash still
  matches the magnet.
- **DHT (BEP 5) peer discovery and announce.** A KRPC client over UDP: 160-bucket
  routing table, XOR-ordered `get_peers` hunts with token capture from every node
  that hands one, and `announce_peer` to the token-holding closest nodes after a
  download. Trackerless magnets resolve through it; `--no-dht` opts out.
- **Seeding (`--seed <secs>`).** After the download completes, the engine opens a
  listener and serves exactly the pieces `verify_existing` passes: MSB-first
  Bitfield, unchoke on interest, 128 KiB request cap, one thread per peer, and
  a report of bytes and peers served. A partially-downloaded directory seeds
  only what verifies — the seeder lies about nothing.
- `--peers` to bypass the tracker and `--max-peers` to tune concurrency.
- 37 tests, including an end-to-end download from a fake local peer, a UDP
  router → holder DHT chain, a seeder/leecher round-trip, and a wrong-info-hash
  rejection.

### Fixed
- `announce_peer` retries a datagram that goes unacknowledged inside its
  budget instead of counting a dropped UDP packet as a silent node; a
  deterministic test drops the first announce to pin the behaviour.

### Removed
- The Node.js engine, Electron app, web UI and their tests. The front ends are
  staged; see the README parity table.

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
