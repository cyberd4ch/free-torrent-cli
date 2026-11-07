<div align="center">

# 🌀 free-torrent-cli

**A lightweight, from-scratch BitTorrent client, now in Rust**

![Rust](https://img.shields.io/badge/Rust-1.75%2B-000000?style=flat-square&logo=rust)
![License](https://img.shields.io/badge/License-MIT-blue?style=flat-square)
![Tests](https://img.shields.io/badge/Tests-28%20passing-brightgreen?style=flat-square)

Speaks the BitTorrent wire protocol directly — no libtorrent, no shortcuts.
Verifies every piece with SHA-1, resumes from disk, and handles multi-file
torrents across file boundaries.

</div>

---

## Status

This is a rewrite of the original Node.js client. The protocol engine and CLI are
ported; the front ends and peer-discovery extras are staged behind them.

| Area | State |
|---|---|
| Bencode codec (with exact `info` byte-span for the info hash) | ✅ |
| `.torrent` parsing, single and multi-file | ✅ |
| Magnet URI parsing (hex + base32 info hashes) | ✅ |
| Metadata exchange (BEP 9/10), so magnet-only downloads | ✅ |
| HTTP tracker announces (compact + dictionary peers) | ✅ |
| UDP tracker announces (BEP 15) | ✅ |
| Peer wire protocol (handshake, bitfield, choke, request/piece) | ✅ |
| SHA-1 piece verification, corrupt pieces re-queued | ✅ |
| Multi-file storage mapping, directory creation | ✅ |
| Resume: verify existing pieces on disk | ✅ |
| Tracker re-announce / peer refill when peers run dry | ✅ |
| CLI with live progress | ✅ |
| DHT (BEP 5): get_peers hunting + announce | ✅ |
| Seeding / upload slots | ⬜ not yet |
| Desktop (Electron) and web UI | ⬜ not yet |

Magnet links download today, tracker or not: a trackerless magnet hunts its peers
through the DHT (BEP 5), then announces itself to the closest nodes when done.

## Quick start

```bash
cargo build --release
./target/release/ftc ./some.torrent -o ./downloads
```

```
USAGE:
    ftc <file.torrent | magnet:?xt=urn:btih:...> [options]

OPTIONS:
    -o, --output <dir>    where files are written (default: current directory)
        --peers <list>    comma-separated host:port peers, skipping the tracker
        --max-peers <n>   simultaneous peer connections (default: 6)
        --no-dht          skip DHT peer discovery (trackers only)
```

Progress is printed in place:

```
[██████████████░░░░░░░░░░░░░░░░]  47%  9/19  1.8 MiB/s  4 peers
```

## Architecture

```
crates/core     protocol engine (library)
  bencode.rs    encoder/decoder; records the raw info-dict bytes for hashing
  torrent.rs    metainfo parsing, piece table, file layout, sanitised paths
  magnet.rs     magnet URIs, hex and base32 info hashes
  tracker.rs    HTTP + UDP announces, compact peer decoding
  dht.rs        BEP 5 peer discovery: KRPC, routing table, announce
  peer.rs       wire framing and message codec
  storage.rs    logical stream → files, piece writes, resume verification
  session.rs    peer workers, retrying pieces, progress accounting
crates/cli      the `ftc` binary
```

Design notes that differ from the Node version:

- **Info hash correctness.** The hash is computed over the original `info` dictionary
  bytes, not a re-encoded value, so unusual key ordering cannot change the torrent identity.
- **Path safety.** Torrent paths are rejected if any component is `..`, absolute, or
  contains a separator; a hostile `.torrent` cannot write outside the output directory.
- **Bounded frames.** Peer frames are capped at 2 MiB before allocation, so a malicious
  length header cannot exhaust memory.
- **DHT as a client.** The node discovers and announces but does not service inbound
  queries yet; a get_peers token is kept from any node that hands one, values or not —
  the closest nodes for a torrent usually do not hold values themselves.
- **Fail fast, retry narrowly.** A piece that fails SHA-1 goes back on the queue; a peer
  that misbehaves is dropped. The run fails only after four refill attempts.

## Testing

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

The suite includes end-to-end tests: a fake peer on localhost serves a real torrent and
the session downloads, verifies and writes it, and a fake router → holder chain over
UDP exercises a full DHT hunt and a token-validated announce.

## Roadmap

1. Seeding and upload slots.
2. A local HTTP API, then the web and desktop front ends on top of it.

## License

MIT
