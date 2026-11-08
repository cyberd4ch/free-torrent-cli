//! Tracker announces: HTTP (compact peers) and UDP (BEP 15).

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use percent_encoding::{percent_encode, NON_ALPHANUMERIC};
use rand::Rng;
use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    None,
    Started,
    Completed,
    Stopped,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Event::None => "",
            Event::Started => "started",
            Event::Completed => "completed",
            Event::Stopped => "stopped",
        }
    }

    fn as_u32(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AnnounceRequest {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
}

impl AnnounceRequest {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20], total: u64, left: u64) -> Self {
        AnnounceRequest {
            info_hash,
            peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: total.saturating_sub(left),
            left,
            event: Event::Started,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AnnounceResponse {
    pub interval: u64,
    pub peers: Vec<SocketAddr>,
}

pub fn announce(url: &str, request: &AnnounceRequest) -> Result<AnnounceResponse> {
    if url.starts_with("udp://") {
        announce_udp(url, request)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        announce_http(url, request)
    } else {
        Err(Error::Tracker(format!("unsupported tracker scheme: {url}")))
    }
}

fn announce_http(url: &str, request: &AnnounceRequest) -> Result<AnnounceResponse> {
    let separator = if url.contains('?') { '&' } else { '?' };
    let info_hash = percent_encode(&request.info_hash, NON_ALPHANUMERIC);
    let peer_id = percent_encode(&request.peer_id, NON_ALPHANUMERIC);
    let event = request.event.as_str();
    let query = format!(
        "{separator}info_hash={info_hash}&peer_id={peer_id}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=200&event={event}",
        request.port, request.uploaded, request.downloaded, request.left
    );

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let response = agent
        .get(&format!("{url}{query}"))
        .call()
        .map_err(|err| Error::Tracker(format!("{url}: {err}")))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(4 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|err| Error::Tracker(err.to_string()))?;
    parse_http_response(&body)
}

pub fn parse_http_response(body: &[u8]) -> Result<AnnounceResponse> {
    let value = bencode::decode(body)?;
    if let Some(reason) = value.get("failure reason").and_then(Value::as_str) {
        return Err(Error::Tracker(reason.to_string()));
    }
    let interval = value
        .get("interval")
        .and_then(Value::as_int)
        .unwrap_or(1800)
        .max(1) as u64;
    let mut peers = Vec::new();

    if let Some(compact) = value.get("peers").and_then(Value::as_bytes) {
        for chunk in compact.as_chunks::<6>().0 {
            let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            if port != 0 {
                peers.push(SocketAddr::from((ip, port)));
            }
        }
    }

    if let Some(list) = value.get("peers").and_then(Value::as_list) {
        for entry in list {
            let Some(ip) = entry.get("ip").and_then(Value::as_str) else {
                continue;
            };
            let Some(port) = entry.get("port").and_then(Value::as_int) else {
                continue;
            };
            if let Ok(ip) = ip.parse::<std::net::IpAddr>() {
                if let Ok(port) = u16::try_from(port) {
                    peers.push(SocketAddr::new(ip, port));
                }
            }
        }
    }

    Ok(AnnounceResponse { interval, peers })
}

fn announce_udp(url: &str, request: &AnnounceRequest) -> Result<AnnounceResponse> {
    let address = resolve_udp(url)?;
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).map_err(|err| Error::Tracker(err.to_string()))?;
    socket
        .connect(address)
        .map_err(|err| Error::Tracker(err.to_string()))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|err| Error::Tracker(err.to_string()))?;

    let mut rng = rand::thread_rng();
    let transaction: u32 = rng.gen();

    let mut connect = Vec::with_capacity(16);
    connect.extend_from_slice(&0x41727101980u64.to_be_bytes());
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&transaction.to_be_bytes());
    socket
        .send(&connect)
        .map_err(|err| Error::Tracker(err.to_string()))?;

    let mut buf = [0u8; 2048];
    let len = socket
        .recv(&mut buf)
        .map_err(|err| Error::Tracker(format!("connect timed out: {err}")))?;
    if len < 16 {
        return Err(Error::Tracker("short UDP connect response".into()));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let echoed = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if action != 0 || echoed != transaction {
        return Err(Error::Tracker("unexpected UDP connect response".into()));
    }
    let connection_id = u64::from_be_bytes(buf[8..16].try_into().unwrap());

    let announce_transaction: u32 = rng.gen();
    let mut packet = Vec::with_capacity(98);
    packet.extend_from_slice(&connection_id.to_be_bytes());
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.extend_from_slice(&announce_transaction.to_be_bytes());
    packet.extend_from_slice(&request.info_hash);
    packet.extend_from_slice(&request.peer_id);
    packet.extend_from_slice(&request.downloaded.to_be_bytes());
    packet.extend_from_slice(&request.left.to_be_bytes());
    packet.extend_from_slice(&request.uploaded.to_be_bytes());
    packet.extend_from_slice(&request.event.as_u32().to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&rng.gen::<u32>().to_be_bytes());
    packet.extend_from_slice(&(-1i32).to_be_bytes());
    packet.extend_from_slice(&request.port.to_be_bytes());
    socket
        .send(&packet)
        .map_err(|err| Error::Tracker(err.to_string()))?;

    let len = socket
        .recv(&mut buf)
        .map_err(|err| Error::Tracker(format!("announce timed out: {err}")))?;
    if len < 20 {
        return Err(Error::Tracker("short UDP announce response".into()));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let echoed = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if echoed != announce_transaction {
        return Err(Error::Tracker("UDP announce transaction mismatch".into()));
    }
    if action == 3 {
        let message = String::from_utf8_lossy(&buf[8..]).to_string();
        return Err(Error::Tracker(message));
    }
    if action != 1 {
        return Err(Error::Tracker(format!("unexpected UDP action {action}")));
    }
    let interval = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as u64;
    let peers = buf[20..len]
        .as_chunks::<6>()
        .0
        .iter()
        .filter_map(|chunk| {
            let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            (port != 0).then(|| SocketAddr::from((ip, port)))
        })
        .collect();

    Ok(AnnounceResponse {
        interval: interval.max(1),
        peers,
    })
}

fn resolve_udp(url: &str) -> Result<SocketAddr> {
    let rest = url
        .strip_prefix("udp://")
        .ok_or_else(|| Error::Tracker("not a UDP URL".into()))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    authority
        .to_socket_addrs()
        .map_err(|err| Error::Tracker(format!("cannot resolve {authority}: {err}")))?
        .next()
        .ok_or_else(|| Error::Tracker(format!("no address for {authority}")))
}

/// Raw UDP announce buffer helper, exposed so the packet layout can be tested.
#[doc(hidden)]
pub fn udp_announce_packet(
    connection_id: u64,
    transaction: u32,
    request: &AnnounceRequest,
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(98);
    packet.extend_from_slice(&connection_id.to_be_bytes());
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.extend_from_slice(&transaction.to_be_bytes());
    packet.extend_from_slice(&request.info_hash);
    packet.extend_from_slice(&request.peer_id);
    packet.extend_from_slice(&request.downloaded.to_be_bytes());
    packet.extend_from_slice(&request.left.to_be_bytes());
    packet.extend_from_slice(&request.uploaded.to_be_bytes());
    packet.extend_from_slice(&request.event.as_u32().to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&(-1i32).to_be_bytes());
    packet.extend_from_slice(&request.port.to_be_bytes());
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::encode;

    fn compact(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut out = ip.to_vec();
        out.extend_from_slice(&port.to_be_bytes());
        out
    }

    #[test]
    fn parses_compact_peer_list() {
        let mut peers = compact([10, 0, 0, 1], 6881);
        peers.extend(compact([192, 168, 1, 5], 51413));
        let body = encode(&Value::Dict(vec![
            (b"interval".to_vec(), Value::Int(900)),
            (b"peers".to_vec(), Value::Bytes(peers)),
        ]));
        let response = parse_http_response(&body).unwrap();
        assert_eq!(response.interval, 900);
        assert_eq!(response.peers.len(), 2);
        assert_eq!(response.peers[1].port(), 51413);
    }

    #[test]
    fn parses_dictionary_peer_list() {
        let body = encode(&Value::Dict(vec![
            (b"interval".to_vec(), Value::Int(60)),
            (
                b"peers".to_vec(),
                Value::List(vec![Value::Dict(vec![
                    (b"ip".to_vec(), Value::Bytes(b"127.0.0.1".to_vec())),
                    (b"port".to_vec(), Value::Int(7000)),
                ])]),
            ),
        ]));
        let response = parse_http_response(&body).unwrap();
        assert_eq!(response.peers.len(), 1);
        assert_eq!(response.peers[0].to_string(), "127.0.0.1:7000");
    }

    #[test]
    fn surfaces_tracker_failure_reason() {
        let body = encode(&Value::Dict(vec![(
            b"failure reason".to_vec(),
            Value::Bytes(b"unregistered torrent".to_vec()),
        )]));
        let error = parse_http_response(&body).unwrap_err().to_string();
        assert!(error.contains("unregistered torrent"));
    }

    #[test]
    fn builds_standard_udp_announce_packet() {
        let request = AnnounceRequest::new([1u8; 20], [2u8; 20], 100, 40);
        let packet = udp_announce_packet(7, 9, &request);
        assert_eq!(packet.len(), 98);
        assert_eq!(u64::from_be_bytes(packet[0..8].try_into().unwrap()), 7);
        assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(packet[12..16].try_into().unwrap()), 9);
        assert_eq!(&packet[16..36], &[1u8; 20]);
        assert_eq!(u64::from_be_bytes(packet[56..64].try_into().unwrap()), 60);
        assert_eq!(u32::from_be_bytes(packet[80..84].try_into().unwrap()), 2);
    }
}
