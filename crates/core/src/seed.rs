//! Seeding: serving pieces we verified on disk to whoever connects.
//!
//! A seeder is also where magnet links get their start: the first peer with
//! the metadata is usually a seed, so the serve loop answers BEP 9
//! `ut_metadata` exchanges alongside ordinary piece requests.

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use crate::metadata;
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

/// Our BEP 9 announcements: which local id answers `ut_metadata`, and how
/// big the metadata blobs we can serve are.
fn metadata_handshake(total: usize) -> Vec<u8> {
    bencode::encode(&Value::Dict(vec![
        (
            b"m".to_vec(),
            Value::Dict(vec![(
                metadata::UT_METADATA.to_vec(),
                Value::Int(metadata::OUR_METADATA_ID as i64),
            )]),
        ),
        (b"metadata_size".to_vec(), Value::Int(total as i64)),
        (b"v".to_vec(), Value::Bytes(b"ftc 3.0".to_vec())),
    ]))
}

/// One BEP 9 data message: the bencoded header carries the type, the piece
/// number and the total size; the bytes of the piece follow immediately.
fn metadata_data(piece: usize, raw: &[u8]) -> Vec<u8> {
    let start = piece * metadata::METADATA_PIECE;
    let end = (start + metadata::METADATA_PIECE).min(raw.len());
    let block = raw
        .get(start..end)
        .unwrap_or_else(|| raw.get(raw.len()..).unwrap_or(&[]));
    bencode::encode(&Value::Dict(vec![
        (b"msg_type".to_vec(), Value::Int(1)),
        (b"piece".to_vec(), Value::Int(piece as i64)),
        (b"total_size".to_vec(), Value::Int(raw.len() as i64)),
    ]))
    .into_iter()
    .chain(block.iter().copied())
    .collect()
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
    // Same reason as the downloader side: block requests should leave unbuffered.
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(PEER_IDLE))?;
    // The downloader speaks first; we mirror the handshake and stay only if
    // both sides agree on which torrent this socket is about.
    peer::read_handshake(&mut stream, &torrent.info_hash)
        .map_err(|err| Error::Peer(format!("handshake refused: {err}")))?;
    stream.set_write_timeout(Some(PEER_IDLE))?;
    peer::write_handshake(&mut stream, &torrent.info_hash, &self_id)?;
    totals.peers.fetch_add(1, Ordering::Relaxed);

    // Magnet fetchers speak BEP 10 first: remember which local id they
    // listen on for metadata, and introduce our own in return.
    let mut peer_ut: Option<u8> = None;

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
            Message::Extended(id, payload) => {
                if id == 0 {
                    // Their extension handshake: pick up their metadata id,
                    // then advertise ours along with the metadata size.
                    if peer_ut.is_none() {
                        peer_ut = metadata::parse_extension_handshake(&payload)
                            .ok()
                            .and_then(|(id, _)| id);
                    }
                    peer::write_message(
                        &mut stream,
                        &Message::Extended(0, metadata_handshake(torrent.raw_info.len())),
                    )?;
                } else if let Ok((header, used)) = bencode::decode_prefix(&payload) {
                    // A ut_metadata exchange addressed to us: requests get
                    // data, rejects and unknown types are ignored.
                    let kind = header.get("msg_type").and_then(Value::as_int).unwrap_or(-1);
                    if kind == 0 {
                        // Replies are addressed with the peer's advertised id
                        // (BEP 10): each side picks the id the other must use.
                        let reply_id = peer_ut.unwrap_or(id);
                        let piece = header.get("piece").and_then(Value::as_int).unwrap_or(-1);
                        let in_range = piece >= 0
                            && (piece as usize) * metadata::METADATA_PIECE < torrent.raw_info.len();
                        if in_range {
                            peer::write_message(
                                &mut stream,
                                &Message::Extended(
                                    reply_id,
                                    metadata_data(piece as usize, &torrent.raw_info),
                                ),
                            )?;
                        } else {
                            peer::write_message(
                                &mut stream,
                                &Message::Extended(
                                    reply_id,
                                    bencode::encode(&Value::Dict(vec![
                                        (b"msg_type".to_vec(), Value::Int(2)),
                                        (b"piece".to_vec(), Value::Int(piece)),
                                    ])),
                                ),
                            )?;
                        }
                    }
                    let _ = used;
                }
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

    /// A ut_metadata fetcher: handshake, announce a metadata id, read ours,
    /// request piece 0, and compare the bytes against the real info dict.
    fn fake_magnet(port: u16, info_hash: &[u8; 20], expected: &[u8]) -> bool {
        let mut maybe = None;
        for _ in 0..60 {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                maybe = Some(stream);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut stream = match maybe {
            Some(stream) => stream,
            None => return false,
        };
        let my_id = *b"-FT0300-magnet000000";
        if peer::write_handshake(&mut stream, info_hash, &my_id).is_err() {
            return false;
        }
        if peer::read_handshake(&mut stream, info_hash).is_err() {
            return false;
        }
        let mine = bencode::encode(&Value::Dict(vec![(
            b"m".to_vec(),
            Value::Dict(vec![(b"ut_metadata".to_vec(), Value::Int(7))]),
        )]));
        if peer::write_message(&mut stream, &Message::Extended(0, mine)).is_err() {
            return false;
        }
        // Wait for the seeder's handshake: it names the id our requests must
        // carry, and the size of the metadata it serves.
        let (mut seeder_ut, mut size, mut announced) = (1u8, None, false);
        while !announced {
            match peer::read_message(&mut stream) {
                Ok(Message::Extended(0, payload)) => {
                    if let Ok((header, _)) = bencode::decode_prefix(&payload) {
                        size = header.get("metadata_size").and_then(Value::as_int);
                        seeder_ut = header
                            .get("m")
                            .and_then(|m| m.get("ut_metadata"))
                            .and_then(Value::as_int)
                            .and_then(|id| u8::try_from(id).ok())
                            .unwrap_or(1);
                    }
                    announced = true;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        if size != Some(expected.len() as i64) {
            return false;
        }
        let request = bencode::encode(&Value::Dict(vec![
            (b"msg_type".to_vec(), Value::Int(0)),
            (b"piece".to_vec(), Value::Int(0)),
        ]));
        if peer::write_message(&mut stream, &Message::Extended(seeder_ut, request)).is_err() {
            return false;
        }
        // The data must come back addressed to 7 (our advertised id) with the
        // exact bytes of piece 0.
        loop {
            match peer::read_message(&mut stream) {
                Ok(Message::Extended(id, payload)) => {
                    if id == 0 {
                        continue;
                    }
                    if id != 7 {
                        return false;
                    }
                    if let Ok((header, used)) = bencode::decode_prefix(&payload) {
                        let slice = &payload[used..];
                        return header.get("msg_type").and_then(Value::as_int) == Some(1)
                            && header.get("total_size").and_then(Value::as_int)
                                == Some(expected.len() as i64)
                            && slice == &expected[..slice.len().min(expected.len())];
                    }
                    return false;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn serves_magnet_metadata_to_bep9_fetchers() {
        let (root, storage, torrent, _) = seeded("utmeta", &["seed me with metadata!"]);
        let info_hash = torrent.info_hash;
        let raw = torrent.raw_info.clone();
        const UT_PORT: u16 = 16875;
        let server = std::thread::spawn(move || {
            let _ = serve(storage, torrent, UT_PORT, 2, Duration::from_secs(6));
        });

        let ok = fake_magnet(UT_PORT, &info_hash, &raw);
        assert!(ok, "the magnet fetcher received the info dictionary");

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&root);
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
