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
use sha1::{Digest, Sha1};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub const METADATA_PIECE: usize = 16 * 1024;
const MAX_METADATA: usize = 8 * 1024 * 1024;
const UT_METADATA: &[u8] = b"ut_metadata";
/// Waiting time for one trackerless-magnet DHT hunt.
const DHT_HUNT: Duration = Duration::from_secs(15);

pub fn extension_handshake(port: u16) -> Vec<u8> {
    bencode::encode(&Value::Dict(vec![
        (
            b"m".to_vec(),
            Value::Dict(vec![(UT_METADATA.to_vec(), Value::Int(1))]),
        ),
        (b"v".to_vec(), Value::Bytes(b"ftc 3.0".to_vec())),
        (b"reqq".to_vec(), Value::Int(64)),
        (b"p".to_vec(), Value::Int(port as i64)),
    ]))
}

fn parse_extension_handshake(payload: &[u8]) -> Result<(Option<u8>, Option<usize>)> {
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
            Message::KeepAlive | Message::Have(_) | Message::Bitfield(_) => {}
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
                Message::Extended(id, payload) if id == ut_id => {
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
                Message::Extended(0, _) | Message::KeepAlive | Message::Have(_) => {}
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
/// metadata; a trackerless magnet falls back to the DHT (BEP 5).
pub fn fetch(
    trackers: &[String],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    use_dht: bool,
) -> Result<Vec<u8>> {
    let mut peers = Vec::new();
    let mut errors = Vec::new();
    if trackers.is_empty() && !use_dht {
        return Err(Error::Tracker(
            "magnet has no trackers and DHT discovery is disabled".into(),
        ));
    }
    if trackers.is_empty() {
        eprintln!("magnet has no trackers — hunting peers via the DHT");
        let node = Dht::boot(dht::random_node_id(), port)
            .map_err(|err| Error::Tracker(format!("DHT bootstrap failed: {err}")))?;
        let found = node.peer_hunt(*info_hash, 30, DHT_HUNT);
        if found.is_empty() {
            return Err(Error::Tracker(
                "magnet has no trackers and the DHT found no peers".into(),
            ));
        }
        peers.extend(found);
    }

    let request = AnnounceRequest::new(*info_hash, *peer_id, 0, METADATA_PIECE as u64);
    for url in trackers {
        match tracker::announce(url, &request) {
            Ok(response) => peers.extend(response.peers.into_iter().filter(|peer| peer.is_ipv4())),
            Err(err) => errors.push(format!("{url}: {err}")),
        }
    }
    let mut seen = std::collections::HashSet::new();
    peers.retain(|peer| seen.insert(*peer));

    let mut last = errors.join("; ");
    for peer in peers.iter().take(30) {
        match fetch_from_peer(*peer, info_hash, peer_id, port) {
            Ok(metadata) => return Ok(metadata),
            Err(err) => last = format!("{peer}: {err}"),
        }
    }
    Err(Error::Peer(if last.is_empty() {
        "no peers returned metadata".into()
    } else {
        last
    }))
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
        let _ = peer::write_message(&mut stream, &Message::Extended(0, handshake));

        while let Ok(message) = peer::read_message(&mut stream) {
            if let Message::Extended(3, payload) = message {
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
                let _ = peer::write_message(&mut stream, &Message::Extended(3, out));
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
