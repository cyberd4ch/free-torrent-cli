//! Seeding: serving pieces we verified on disk to whoever connects.

use crate::error::{Error, Result};
use crate::peer::{self, Message};
use crate::storage::Storage;
use crate::torrent::Torrent;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One peer's read cannot monopolize the disk: a single request is capped
/// at 128 KiB (downloading peers ask for 16 KiB blocks by convention).
const MAX_REQUEST: u32 = 128 * 1024;
/// How long an idle peer (no messages at all) is kept around.
const PEER_IDLE: Duration = Duration::from_secs(30);

/// What one seeding window gave away, for the CLI to report.
#[derive(Debug)]
pub struct SeedReport {
    pub peers: usize,
    pub bytes: u64,
}

/// Cross-thread tally, shared by the per-connection threads. Counting
/// happens as blocks leave the disk, so a peer that hangs up mid-session
/// still leaves behind the bytes it actually took.
#[derive(Debug, Default)]
struct Totals {
    peers: AtomicUsize,
    bytes: AtomicU64,
}

/// Serve from `storage` to inbound peers for `duration`, `max_peers` at a
/// time, listening on `port`.
///
/// Seeding is read-only and honest: the pieces on offer are exactly those
/// `Storage::verify_existing` passes right now — serving anything else
/// would poison the swarm. One thread per connection, so a slow reader
/// holds its socket without slowing anyone else down.
pub fn serve(
    storage: Arc<Storage>,
    torrent: Arc<Torrent>,
    port: u16,
    max_peers: usize,
    duration: Duration,
) -> Result<SeedReport> {
    let have = storage.verify_existing(&torrent)?;
    let offering = have.iter().filter(|complete| **complete).count();
    if offering == 0 {
        return Err(Error::Seed(
            "nothing verified on disk — there is nothing to serve".into(),
        ));
    }

    let listener = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .map_err(|err| Error::Seed(format!("cannot listen on port {port}: {err}")))?;
    // Non-blocking accept polling: the loop sleeps between checks, which
    // keeps the deadline honest even with zero inbound connections.
    listener.set_nonblocking(true)?;
    eprintln!(
        "seeding '{}' — {}/{} pieces on port {} for {}s (max {max_peers} peers)",
        torrent.name,
        offering,
        torrent.piece_count(),
        listener.local_addr()?.port(),
        duration.as_secs(),
    );

    let deadline = Instant::now() + duration;
    let busy = Arc::new(AtomicUsize::new(0));
    let totals: Arc<Totals> = Arc::default();
    let self_id = crate::session::make_peer_id();

    while Instant::now() < deadline {
        if busy.load(Ordering::Relaxed) >= max_peers {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        match listener.accept() {
            Ok((stream, address)) => {
                busy.fetch_add(1, Ordering::Relaxed);
                let storage = Arc::clone(&storage);
                let torrent = Arc::clone(&torrent);
                let have = have.clone();
                let busy = Arc::clone(&busy);
                let totals = Arc::clone(&totals);
                std::thread::spawn(move || {
                    if let Err(err) = handle(stream, &storage, &torrent, &have, self_id, &totals) {
                        eprintln!("seed: peer {address}: {err}");
                    }
                    busy.fetch_sub(1, Ordering::Relaxed);
                });
            }
            // Empty poll cycle: nobody is connecting yet.
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(Error::Seed(format!("accept failed: {err}"))),
        }
    }

    Ok(SeedReport {
        peers: totals.peers.load(Ordering::Relaxed),
        bytes: totals.bytes.load(Ordering::Relaxed),
    })
}

/// One peer, handshake to goodbye. Totals are updated as we go.
fn handle(
    mut stream: TcpStream,
    storage: &Storage,
    torrent: &Torrent,
    have: &[bool],
    self_id: [u8; 20],
    totals: &Totals,
) -> Result<()> {
    // Accepted sockets inherit the listener's non-blocking flag on BSD/macOS;
    // undo it or every read comes back as EAGAIN (os error 35).
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(PEER_IDLE))?;
    // The downloader speaks first; we mirror the handshake and stay only if
    // both sides agree on which torrent this socket is about.
    peer::read_handshake(&mut stream, &torrent.info_hash)
        .map_err(|err| Error::Peer(format!("handshake refused: {err}")))?;
    stream.set_write_timeout(Some(PEER_IDLE))?;
    peer::write_handshake(&mut stream, &torrent.info_hash, &self_id)?;
    totals.peers.fetch_add(1, Ordering::Relaxed);

    // The bitfield of what we hold, MSB-first like every BitTorrent client.
    let mut bitfield = Vec::with_capacity(have.len().div_ceil(8));
    for chunk in have.chunks(8) {
        let mut byte = 0u8;
        for (position, complete) in chunk.iter().enumerate() {
            if *complete {
                byte |= 0x80 >> position;
            }
        }
        bitfield.push(byte);
    }
    peer::write_message(&mut stream, &Message::Bitfield(bitfield))?;

    loop {
        let message = match peer::read_message(&mut stream) {
            Ok(message) => message,
            // EOF / timeout / reset: the peer hung up. A normal goodbye,
            // not a failure.
            Err(_) => return Ok(()),
        };
        match message {
            Message::Interested => {
                // A seeder unchokes everyone it has pieces for.
                peer::write_message(&mut stream, &Message::Unchoke)?;
            }
            Message::Request {
                index,
                begin,
                length,
            } => {
                let (index, begin, length) = (index as usize, begin as u64, length as u64);
                let serveable = index < have.len()
                    && have[index]
                    && 0 < length
                    && length <= MAX_REQUEST as u64
                    && begin + length <= torrent.piece_size(index);
                if !serveable {
                    return Err(Error::Peer(format!(
                        "dropping peer after unserveable request \
                         (piece {index}, begin {begin}, length {length})"
                    )));
                }
                let start = index as u64 * torrent.piece_length + begin;
                let mut block = vec![0u8; length as usize];
                storage.read_at(start, &mut block)?;
                peer::write_message(
                    &mut stream,
                    &Message::Piece {
                        index: index as u32,
                        begin: begin as u32,
                        data: block,
                    },
                )?;
                totals.bytes.fetch_add(length, Ordering::Relaxed);
            }
            // Bitfields and haves from neighboring seeders, chokes, cancels
            // and exotic messages — nothing here asks us for disk.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::tests::single_file_torrent;
    use std::io::Read;
    use std::path::PathBuf;

    /// The two tests run concurrently and each gets its own port — also both
    /// get *different* data, so even a stray cross-connection fails over
    /// the handshake instead of trading blocks.
    const TEST_PORT: u16 = 16873;
    const ALT_PORT: u16 = 16871;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ftc-seed-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A complete three-piece torrent on disk, ready to serve.
    fn seeded(tag: &str, data: &[&str]) -> (PathBuf, Arc<Storage>, Arc<Torrent>, Vec<u8>) {
        let data: Vec<u8> = data.concat().as_bytes().to_vec();
        let raw = single_file_torrent("data.bin", &data, 8);
        let torrent = Torrent::parse(&raw).unwrap();
        let root = temp_root(tag);
        let storage = Storage::open(&root, &torrent).unwrap();
        for index in 0..torrent.piece_count() {
            let start = index as u64 * torrent.piece_length;
            let size = torrent.piece_size(index) as usize;
            storage
                .write_piece(
                    index,
                    torrent.piece_length,
                    &data[start as usize..start as usize + size],
                )
                .unwrap();
        }
        assert!(storage
            .verify_existing(&torrent)
            .unwrap()
            .iter()
            .all(|ok| *ok));
        (root, Arc::new(storage), Arc::new(torrent), data)
    }

    /// A minimal leecher: handshake, take the Bitfield, get unchoked, fetch
    /// one block, then fire an impossible request. True only when the block
    /// arrived exactly and the server shut us down for the impossible one.
    fn fake_leecher(port: u16, info_hash: &[u8; 20], data: &[u8]) -> bool {
        let mut maybe = None;
        for _ in 0..60 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => {
                    maybe = Some(stream);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let mut stream = match maybe {
            Some(stream) => stream,
            None => return false,
        };
        let my_id = *b"-FT0300-leecher00000";
        if peer::write_handshake(&mut stream, info_hash, &my_id).is_err() {
            return false;
        }
        if peer::read_handshake(&mut stream, info_hash).is_err() {
            return false;
        }
        match peer::read_message(&mut stream) {
            Ok(Message::Bitfield(_)) => {}
            _ => return false,
        }
        if peer::write_message(&mut stream, &Message::Interested).is_err() {
            return false;
        }
        match peer::read_message(&mut stream) {
            Ok(Message::Unchoke) => {}
            _ => return false,
        }
        if peer::write_message(
            &mut stream,
            &Message::Request {
                index: 1,
                begin: 0,
                length: 8,
            },
        )
        .is_err()
        {
            return false;
        }
        match peer::read_message(&mut stream) {
            Ok(Message::Piece { data: block, .. }) if block == data[8..16].to_vec() => {}
            _ => return false,
        }
        // Piece index far past the end of the torrent...
        if peer::write_message(
            &mut stream,
            &Message::Request {
                index: 9,
                begin: 0,
                length: 4,
            },
        )
        .is_err()
        {
            return false;
        }
        // ...and the server drops the connection: EOF, not a message.
        matches!(stream.read(&mut [0u8; 1]), Ok(0) | Err(_))
    }

    #[test]
    fn serves_verified_pieces_and_drops_impossible_requests() {
        // 19 bytes → pieces of 8, 8 and a 3-byte tail; different data than
        // the wrong-hash test below, so the info hashes cannot collide.
        let (root, storage, torrent, data) = seeded("round-trip", &["seed me! 0123", "456789"]);
        let info_hash = torrent.info_hash;
        let client = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            fake_leecher(TEST_PORT, &info_hash, &data)
        });
        let report = serve(storage, torrent, TEST_PORT, 4, Duration::from_secs(6)).unwrap();

        assert!(client.join().unwrap(), "the fake leecher got its block");
        assert_eq!(
            (report.peers, report.bytes),
            (1, 8),
            "one served peer, exactly the requested block"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drops_peers_holding_the_wrong_info_hash() {
        let (root, storage, torrent, _) = seeded("wrong-hash", &["wrong hash seed data!"]);
        let client = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            // Handshake for a different torrent: the server closes the socket,
            // so reading its handshake side hits EOF immediately.
            let mut stream = loop {
                if let Ok(stream) = TcpStream::connect(("127.0.0.1", ALT_PORT)) {
                    break stream;
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let wrong_hash = [9u8; 20];
            let my_id = *b"-FT0300-wronghash000";
            peer::write_handshake(&mut stream, &wrong_hash, &my_id).unwrap();
            peer::read_handshake(&mut stream, &wrong_hash).is_err()
        });
        let report = serve(storage, torrent, ALT_PORT, 4, Duration::from_secs(5)).unwrap();

        assert!(client.join().unwrap(), "the wrong-hash peer got rejected");
        assert_eq!((report.peers, report.bytes), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }
}
