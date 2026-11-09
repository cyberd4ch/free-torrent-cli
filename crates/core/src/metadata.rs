//! Metadata exchange (BEP 9/10): fetch the `info` dictionary for a magnet link.
//!
//! A magnet only carries the info hash, so the torrent's structure has to come from a
//! peer. The assembled bytes are trusted only after their SHA-1 matches the hash in the
//! magnet — the hash is the root of trust, not the peer.

use crate::bencode::{self, Value};
use crate::dht::{self, Dht};
use crate::error::{Error, Result};
use crate::peer::{self, Message};
use crate::tracker::{self, AnnounceRequest};
use rand::seq::SliceRandom;
use sha1::{Digest, Sha1};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub const METADATA_PIECE: usize = 16 * 1024;
const MAX_METADATA: usize = 8 * 1024 * 1024;
pub(crate) const UT_METADATA: &[u8] = b"ut_metadata";
/// The extension id we advertise for messages peers send *to us*. BEP 10 has
/// each side pick the id the other must use, so replies to our metadata
/// requests arrive under this id — not under the peer's advertised id, which is
/// the id we must use when sending requests to them.
pub(crate) const OUR_METADATA_ID: u8 = 1;
/// Waiting time for one trackerless-magnet DHT hunt, and a shorter second
/// pass when the first one only reaches a couple of nodes.
const DHT_HUNT: Duration = Duration::from_secs(15);
const DHT_RETRY: Duration = Duration::from_secs(10);
/// Public trackers tried when a magnet's own announce list is dead (403, no
/// answer, an I2P name that will not resolve). Without this an old magnet with
/// rotting trackers looked like a broken client, not a dead announce list.
const FALLBACK_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
];
/// Upper bound on one metadata resolution, across every peer we try.
const METADATA_BUDGET: Duration = Duration::from_secs(120);
/// Per-tracker wait, and the cap for the whole announce list.
const TRACKER_TIMEOUT: Duration = Duration::from_secs(8);
const TRACKER_BUDGET: Duration = Duration::from_secs(20);
/// How long one batch of metadata exchanges may take. A serving peer answers in
/// well under a second, so a shorter cap tries more of a large swarm instead of
/// spending the whole budget on the first few silent hosts.
const PEER_TIMEOUT: Duration = Duration::from_secs(8);
/// Peers contacted in parallel per batch.
const PEER_BATCH: usize = 8;

pub fn extension_handshake(port: u16) -> Vec<u8> {
    bencode::encode(&Value::Dict(vec![
        (
            b"m".to_vec(),
            Value::Dict(vec![(
                UT_METADATA.to_vec(),
                Value::Int(OUR_METADATA_ID as i64),
            )]),
        ),
        (b"v".to_vec(), Value::Bytes(b"ftc 3.0".to_vec())),
        (b"reqq".to_vec(), Value::Int(64)),
        (b"p".to_vec(), Value::Int(port as i64)),
    ]))
}

pub(crate) fn parse_extension_handshake(payload: &[u8]) -> Result<(Option<u8>, Option<usize>)> {
    let value = bencode::decode(payload)?;
    let id = value
        .get("m")
        .and_then(|map| map.get(std::str::from_utf8(UT_METADATA).unwrap()))
        .and_then(Value::as_int)
        .and_then(|id| u8::try_from(id).ok());
    let size = value
        .get("metadata_size")
        .and_then(Value::as_int)
        .and_then(|size| usize::try_from(size).ok());
    Ok((id, size))
}

fn metadata_request(piece: usize) -> Vec<u8> {
    bencode::encode(&Value::Dict(vec![
        (b"msg_type".to_vec(), Value::Int(0)),
        (b"piece".to_vec(), Value::Int(piece as i64)),
    ]))
}

/// Download the raw `info` dictionary from one peer and verify it against `info_hash`.
pub fn fetch_from_peer(
    address: SocketAddr,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;

    peer::write_handshake(&mut stream, info_hash, peer_id)?;
    peer::read_handshake(&mut stream, info_hash)?;
    peer::write_message(
        &mut stream,
        &Message::Extended(0, extension_handshake(port)),
    )?;

    let (ut_id, size) = loop {
        match peer::read_message(&mut stream)? {
            Message::Extended(0, payload) => {
                let (id, size) = parse_extension_handshake(&payload)?;
                break (id, size);
            }
            // Real clients send ordinary wire traffic first (a bitfield, an
            // unchoke); only a message we cannot handle is a reason to give up.
            Message::KeepAlive
            | Message::Choke
            | Message::Unchoke
            | Message::Interested
            | Message::NotInterested
            | Message::Have(_)
            | Message::Bitfield(_)
            | Message::Port(_)
            | Message::Extended(..) => {}
            other => {
                return Err(Error::Peer(format!(
                    "peer does not speak the extension protocol (message id {})",
                    other.id()
                )))
            }
        }
    };

    let ut_id = ut_id.ok_or_else(|| Error::Peer("peer does not support ut_metadata".into()))?;
    let total = size.ok_or_else(|| Error::Peer("peer did not send metadata_size".into()))?;
    if total == 0 || total > MAX_METADATA {
        return Err(Error::Peer(format!("implausible metadata size {total}")));
    }

    let mut metadata = vec![0u8; total];
    let pieces = total.div_ceil(METADATA_PIECE);
    for index in 0..pieces {
        peer::write_message(
            &mut stream,
            &Message::Extended(ut_id, metadata_request(index)),
        )?;
        loop {
            match peer::read_message(&mut stream)? {
                Message::Extended(id, payload) if id == OUR_METADATA_ID || id == ut_id => {
                    let (header, used) = bencode::decode_prefix(&payload)?;
                    let kind = header.get("msg_type").and_then(Value::as_int).unwrap_or(-1);
                    if kind == 2 {
                        return Err(Error::Peer("peer rejected a metadata request".into()));
                    }
                    if kind != 1 {
                        continue;
                    }
                    let piece = header.get("piece").and_then(Value::as_int).unwrap_or(-1);
                    if piece != index as i64 {
                        continue;
                    }
                    let chunk = &payload[used..];
                    let start = index * METADATA_PIECE;
                    let expected = (total - start).min(METADATA_PIECE);
                    if chunk.len() != expected {
                        return Err(Error::Peer(format!(
                            "metadata piece {index} is {} bytes, expected {expected}",
                            chunk.len()
                        )));
                    }
                    metadata[start..start + chunk.len()].copy_from_slice(chunk);
                    break;
                }
                Message::KeepAlive
                | Message::Choke
                | Message::Unchoke
                | Message::Interested
                | Message::NotInterested
                | Message::Have(_)
                | Message::Bitfield(_)
                | Message::Port(_)
                | Message::Extended(..) => {}
                other => {
                    return Err(Error::Peer(format!(
                        "expected metadata, got message id {}",
                        other.id()
                    )))
                }
            }
        }
    }

    let mut hasher = Sha1::new();
    hasher.update(&metadata);
    if hasher.finalize().as_slice() != info_hash {
        return Err(Error::Peer(
            "assembled metadata does not match the magnet info hash".into(),
        ));
    }
    Ok(metadata)
}

/// Ask trackers for peers and try them in turn until one serves valid
/// metadata; the DHT (BEP 5) is asked whenever the announce list comes back
/// empty, whether the magnet carried no trackers or simply dead ones.
pub struct Resolved {
    /// The raw `info` dictionary, verified against the magnet's hash.
    pub info: Vec<u8>,
    /// Peers the swarm offered, whether or not they served the metadata.
    pub peers: Vec<SocketAddr>,
    /// True when the magnet's own announce list produced no peer, so the
    /// well-known public trackers had to be used instead.
    pub fallback_trackers: bool,
}

pub fn fetch(
    trackers: &[String],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    use_dht: bool,
) -> Result<Vec<u8>> {
    fetch_all(trackers, info_hash, peer_id, port, use_dht).map(|resolved| resolved.info)
}

/// What a swarm looks like right now: who the announce list knows, who the DHT
/// knows, and how many of those peers accept a TCP connection at all.
pub struct SwarmReport {
    pub peers: Vec<SocketAddr>,
    pub reachable: usize,
    pub trackers_ok: usize,
    pub tracker_errors: Vec<String>,
    pub dht_peers: usize,
    pub fallback_trackers: bool,
}

/// Diagnose a swarm without downloading metadata: announce, ask the DHT, then
/// probe every discovered peer with a short TCP connect. A torrent that reports
/// 34 peers and 0 reachable is dead, not misconfigured on this end.
pub fn swarm(
    trackers: &[String],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    use_dht: bool,
) -> SwarmReport {
    let request = AnnounceRequest::new(*info_hash, *peer_id, 0, 0);
    let (mut peers, mut trackers_ok, mut tracker_errors) =
        announce_all(trackers, &request, TRACKER_BUDGET);
    let mut fallback_trackers = false;
    if peers.is_empty() && use_dht {
        let extra: Vec<String> = FALLBACK_TRACKERS
            .iter()
            .filter(|url| !trackers.iter().any(|tracker| tracker == **url))
            .map(|url| (*url).to_string())
            .collect();
        if !extra.is_empty() {
            fallback_trackers = true;
            let (found, ok, errors) = announce_all(&extra, &request, Duration::from_secs(10));
            peers.extend(found);
            trackers_ok += ok;
            tracker_errors.extend(errors);
        }
    }

    let mut dht_peers = 0;
    if use_dht {
        if let Ok(found) = from_dht(*info_hash, port) {
            dht_peers = found.len();
            peers.extend(found);
        }
    }
    let mut seen = std::collections::HashSet::new();
    peers.retain(|peer| seen.insert(*peer));
    let reachable = probe_reachable(&peers, Duration::from_secs(2));
    SwarmReport {
        peers,
        reachable,
        trackers_ok,
        tracker_errors,
        dht_peers,
        fallback_trackers,
    }
}

/// TCP-connect every peer in parallel batches; only a completed handshake-level
/// connect counts, which is enough to separate a stale peer list from a live one.
fn probe_reachable(peers: &[SocketAddr], timeout: Duration) -> usize {
    const BATCH: usize = 32;
    let mut reachable = 0;
    for chunk in peers.chunks(BATCH) {
        let (sender, receiver) = std::sync::mpsc::channel();
        for peer in chunk {
            let peer = *peer;
            let sender = sender.clone();
            std::thread::spawn(move || {
                let ok = std::net::TcpStream::connect_timeout(&peer, timeout).is_ok();
                let _ = sender.send(ok);
            });
        }
        drop(sender);
        let deadline = Instant::now() + timeout + Duration::from_secs(1);
        for _ in 0..chunk.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if let Ok(true) = receiver.recv_timeout(left) {
                reachable += 1;
            }
        }
    }
    reachable
}

/// A tracker that accepts the connection but never answers would otherwise
/// block the whole resolve, leaving the add dialog spinning forever. UDP and
/// HTTP announces both run on a helper thread so the caller keeps a deadline.
fn announce_bounded(
    url: &str,
    request: &AnnounceRequest,
    timeout: std::time::Duration,
) -> Result<tracker::AnnounceResponse> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let url = url.to_string();
    let request = *request;
    std::thread::spawn(move || {
        let _ = sender.send(tracker::announce(&url, &request));
    });
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(Error::Tracker(format!(
            "no answer within {}s",
            timeout.as_secs()
        ))),
    }
}

/// Announce to every URL at once and collect what arrives inside `budget`.
/// Stragglers keep running until their own timeout and their results are dropped.
fn announce_all(
    trackers: &[String],
    request: &AnnounceRequest,
    budget: Duration,
) -> (Vec<SocketAddr>, usize, Vec<String>) {
    let (sender, receiver) = std::sync::mpsc::channel();
    for url in trackers {
        let url = url.clone();
        let request = *request;
        let sender = sender.clone();
        std::thread::spawn(move || {
            let result = announce_bounded(&url, &request, TRACKER_TIMEOUT);
            let _ = sender.send((url, result));
        });
    }
    drop(sender);

    let deadline = Instant::now() + budget;
    let mut peers = Vec::new();
    let mut answered = 0usize;
    let mut errors = Vec::new();
    for _ in 0..trackers.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            errors.push(format!(
                "{} tracker(s) did not answer within {}s",
                trackers.len(),
                budget.as_secs()
            ));
            break;
        }
        match receiver.recv_timeout(left) {
            Ok((_, Ok(response))) => {
                answered += 1;
                peers.extend(response.peers.into_iter().filter(|peer| peer.is_ipv4()))
            }
            Ok((url, Err(err))) => errors.push(format!("{url}: {err}")),
            Err(_) => break,
        }
    }
    (peers, answered, errors)
}

/// Like [`fetch`], but also reports the peers that were discovered, so the UI
/// can show the connection set it is about to use before a byte is written.
pub fn fetch_all(
    trackers: &[String],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    use_dht: bool,
) -> Result<Resolved> {
    if trackers.is_empty() && !use_dht {
        return Err(Error::Tracker(
            "magnet has no trackers and DHT discovery is disabled".into(),
        ));
    }

    let mut peers = Vec::new();
    let mut errors = Vec::new();
    let mut fallback_trackers = false;
    let request = AnnounceRequest::new(*info_hash, *peer_id, 0, METADATA_PIECE as u64);
    // Ask every tracker at once. Asking them in turn let one 403 or one silent
    // UDP tracker eat the shared budget, so later trackers never got a turn and
    // the whole magnet looked dead.
    let (found, _answered, announce_errors) = announce_all(trackers, &request, TRACKER_BUDGET);
    peers.extend(found);
    errors.extend(announce_errors);

    // A magnet whose announce list is all 403s, dead UDP hosts or an I2P name
    // still has a public swarm. Try known-good public trackers before falling
    // back to the DHT, instead of calling the magnet dead.
    if peers.is_empty() && use_dht {
        let extra: Vec<String> = FALLBACK_TRACKERS
            .iter()
            .filter(|url| !trackers.iter().any(|tracker| tracker == **url))
            .map(|url| (*url).to_string())
            .collect();
        if !extra.is_empty() {
            fallback_trackers = true;
            eprintln!(
                "no peers from {} magnet tracker(s) — trying {} public tracker(s)",
                trackers.len(),
                extra.len()
            );
            let (found, _answered, extra_errors) =
                announce_all(&extra, &request, Duration::from_secs(10));
            peers.extend(found);
            errors.extend(extra_errors);
        }
    }

    let mut seen = std::collections::HashSet::new();
    peers.retain(|peer| seen.insert(*peer));
    // Tracker order is swarm order, and the first few entries are often stale;
    // shuffling gives every peer a fair chance inside the budget.
    peers.shuffle(&mut rand::thread_rng());

    // One peer at a time meant a few silent hosts could eat the whole budget.
    // A batch at once turns a 200-peer swarm into an answer in seconds.
    let deadline = Instant::now() + METADATA_BUDGET;
    let mut dht_used = false;
    let mut last = errors.join("; ");
    let mut cursor = 0usize;
    loop {
        while cursor < peers.len() && Instant::now() < deadline {
            let end = (cursor + PEER_BATCH).min(peers.len());
            let (sender, receiver) = std::sync::mpsc::channel();
            for peer in &peers[cursor..end] {
                let peer = *peer;
                let sender = sender.clone();
                let info_hash = *info_hash;
                let peer_id = *peer_id;
                std::thread::spawn(move || {
                    let _ = sender.send((peer, fetch_from_peer(peer, &info_hash, &peer_id, port)));
                });
            }
            drop(sender);

            let wait_until = Instant::now() + PEER_TIMEOUT;
            for _ in cursor..end {
                let left = wait_until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match receiver.recv_timeout(left) {
                    Ok((_peer, Ok(info))) => {
                        return Ok(Resolved {
                            info,
                            peers: peers.clone(),
                            fallback_trackers,
                        })
                    }
                    Ok((peer, Err(err))) => {
                        eprintln!("metadata from {peer} failed: {err}");
                        last = format!("{peer}: {err}");
                    }
                    Err(_) => break,
                }
            }
            cursor = end;
        }

        if Instant::now() >= deadline || dht_used || !use_dht {
            break;
        }
        // Trackers can hand back dead peers; the swarm is still out there, so
        // ask the DHT before calling the magnet dead.
        dht_used = true;
        eprintln!(
            "no metadata yet — asking the DHT for {}",
            hex::encode(info_hash)
        );
        match from_dht(*info_hash, port) {
            Ok(found) => {
                let before = peers.len();
                for peer in found {
                    if seen.insert(peer) {
                        peers.push(peer);
                    }
                }
                if peers.len() == before {
                    errors.push("the DHT returned no new peers".into());
                } else {
                    peers[cursor..].shuffle(&mut rand::thread_rng());
                }
            }
            Err(err) => errors.push(err),
        }
    }

    let summary = if last.is_empty() {
        format!("no peers returned metadata ({} discovered)", peers.len())
    } else {
        format!("tried {cursor} of {} peer(s): {last}", peers.len())
    };
    Err(Error::Peer(summary))
}

/// Boot a DHT node and look for peers. The configured port may already be
/// taken (a second magnet resolving, another client seeding), so retry on an
/// ephemeral port rather than failing the whole resolution.
fn from_dht(info_hash: [u8; 20], port: u16) -> std::result::Result<Vec<SocketAddr>, String> {
    let node = match Dht::boot(dht::random_node_id(), port) {
        Ok(node) => node,
        Err(first) => Dht::boot(dht::random_node_id(), 0).map_err(|second| {
            format!("DHT bootstrap failed on port {port} ({first}) and on a free port ({second})")
        })?,
    };
    let mut found = node.peer_hunt(info_hash, 30, DHT_HUNT);
    // One 15 s pass often reaches only part of the lookup frontier; a second,
    // shorter pass is what turns one peer into a usable swarm.
    if found.len() < 4 {
        found.extend(node.peer_hunt(info_hash, 30, DHT_RETRY));
        found.sort();
        found.dedup();
    }
    if found.is_empty() {
        return Err("the DHT returned no peers".into());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::tests::single_file_torrent;
    use crate::torrent::Torrent;
    use std::net::TcpListener;

    fn peer_serving(listener: TcpListener, metadata: Vec<u8>, info_hash: [u8; 20]) {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        if peer::read_handshake(&mut stream, &info_hash).is_err() {
            return;
        }
        let _ = peer::write_handshake(&mut stream, &info_hash, &[4u8; 20]);
        // Client sends its handshake; our reply advertises metadata at extension id 3.
        let _ = peer::read_message(&mut stream);
        let handshake = bencode::encode(&Value::Dict(vec![
            (
                b"m".to_vec(),
                Value::Dict(vec![(UT_METADATA.to_vec(), Value::Int(3))]),
            ),
            (b"metadata_size".to_vec(), Value::Int(metadata.len() as i64)),
        ]));
        // Real clients chatter before the extension handshake (a bitfield, an
        // unchoke); the fetch must ignore it instead of failing the exchange.
        let _ = peer::write_message(&mut stream, &Message::Bitfield(vec![0xff]));
        let _ = peer::write_message(&mut stream, &Message::Unchoke);
        let _ = peer::write_message(&mut stream, &Message::Extended(0, handshake));

        while let Ok(message) = peer::read_message(&mut stream) {
            if let Message::Extended(id, payload) = message {
                // Requests must arrive on the id *we* advertised, and the reply
                // must go out on the id the client advertised (1): that
                // direction is what real clients honour and what the old test
                // peer got wrong.
                assert_eq!(id, 3, "client sent a request on the peer's advertised id");
                let Ok((header, _)) = bencode::decode_prefix(&payload) else {
                    continue;
                };
                let Some(index) = header.get("piece").and_then(Value::as_int) else {
                    continue;
                };
                let start = index as usize * METADATA_PIECE;
                let end = (start + METADATA_PIECE).min(metadata.len());
                let mut out = bencode::encode(&Value::Dict(vec![
                    (b"msg_type".to_vec(), Value::Int(1)),
                    (b"piece".to_vec(), Value::Int(index)),
                    (b"total_size".to_vec(), Value::Int(metadata.len() as i64)),
                ]));
                out.extend_from_slice(&metadata[start..end]);
                let _ = peer::write_message(&mut stream, &Message::Extended(1, out));
            }
        }
    }

    #[test]
    fn fetches_and_verifies_metadata_over_the_wire() {
        let data = vec![7u8; 200_000];
        let raw = single_file_torrent("big.bin", &data, 32_768);
        let torrent = Torrent::parse(&raw).unwrap();
        let metadata = torrent.raw_info.clone();
        let info_hash = torrent.info_hash;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            std::thread::spawn(move || peer_serving(listener, metadata.clone(), info_hash));

        let fetched =
            fetch_from_peer(address, &info_hash, &[9u8; 20], 6881).expect("metadata fetched");
        let torrent = Torrent::from_info(&fetched, &["udp://tracker.test:1337".into()]).unwrap();
        assert_eq!(torrent.total_length, 200_000);
        assert_eq!(torrent.info_hash, info_hash);
        assert_eq!(torrent.name, "big.bin");
        assert_eq!(
            torrent.trackers,
            vec!["udp://tracker.test:1337".to_string()]
        );

        let _ = server.join();
    }

    #[test]
    fn rejects_metadata_that_does_not_match_the_hash() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = b"d4:name4:fakee".to_vec();
        let server = std::thread::spawn(move || peer_serving(listener, metadata, [1u8; 20]));
        assert!(fetch_from_peer(address, &[1u8; 20], &[9u8; 20], 6881).is_err());
        let _ = server.join();
    }

    #[test]
    fn parses_extension_handshake() {
        let payload = bencode::encode(&Value::Dict(vec![
            (
                b"m".to_vec(),
                Value::Dict(vec![(UT_METADATA.to_vec(), Value::Int(2))]),
            ),
            (b"metadata_size".to_vec(), Value::Int(1234)),
        ]));
        let (id, size) = parse_extension_handshake(&payload).unwrap();
        assert_eq!(id, Some(2));
        assert_eq!(size, Some(1234));
    }
}
