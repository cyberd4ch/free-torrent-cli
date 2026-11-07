//! DHT peer discovery (BEP 5): KRPC over UDP, a 160-bucket routing table,
//! `get_peers` fan-out for peer discovery, and `announce_peer` to the nodes
//! that answered.
//!
//! Deliberate scope (this engine is a downloader, not a full-capacity node):
//! IPv4-only compact nodes (matching the engine's IPv4 peer filter), no NAT
//! hole punching, and inbound `ping`/`find_node`/`get_peers` queries are not
//! serviced yet — the node discovers peers, it does not (yet) volunteer routes.
//! Everything runs synchronously on the caller's thread with a blocking
//! socket, like the rest of the engine.

use crate::bencode::{encode, Value};
use crate::error::{Error, Result};
use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BUCKET_SIZE: usize = 8;
const MAX_PACKET: usize = 8192;
const REPLY_WAIT: Duration = Duration::from_millis(500);
const ALPHA: usize = 4;
const STALE_AFTER: Duration = Duration::from_secs(6);
const BOOTSTRAP_BUDGET: Duration = Duration::from_secs(5);
const ANNOUNCE_BUDGET: Duration = Duration::from_secs(3);

pub const DEFAULT_ROUTERS: &[&str] = &[
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "router.bitcomet.com:6881",
    "dht.transmissionbt.com:6881",
];

pub type NodeId = [u8; 20];

pub fn random_node_id() -> NodeId {
    let mut rng = rand::thread_rng();
    let mut id = [0u8; 20];
    rng.fill(&mut id);
    id
}

// -------------------------------------------------------------- routing -----

fn xor_key(a: &NodeId, b: &NodeId) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (index, (left, right)) in a.iter().zip(b.iter()).enumerate() {
        out[index] = left ^ right;
    }
    out
}

#[derive(Debug, Clone)]
struct Entry {
    id: NodeId,
    address: SocketAddr,
    seen: Instant,
}

#[derive(Debug, Default)]
struct RoutingTable {
    /// Bucket index = position of the first differing bit versus our own id
    /// (0 = most significant XOR bit, 159 = least significant).
    buckets: Vec<Vec<Entry>>,
}

impl RoutingTable {
    fn new() -> Self {
        RoutingTable {
            buckets: vec![Vec::new(); 160],
        }
    }

    fn upsert(&mut self, own: &NodeId, id: NodeId, address: SocketAddr) {
        let index = bucket_index(own, &id);
        let Some(bucket) = self.buckets.get_mut(index) else {
            return;
        };
        if let Some(entry) = bucket.iter_mut().find(|entry| entry.id == id) {
            entry.address = address;
            entry.seen = Instant::now();
            return;
        }
        if bucket.len() < BUCKET_SIZE {
            bucket.push(Entry {
                id,
                address,
                seen: Instant::now(),
            });
            return;
        }
        // Bucket full: the least-recently-seen entry yields its slot so
        // traffic keeps the warm routes.
        if let Some(position) = bucket
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.seen)
            .map(|(position, _)| position)
        {
            bucket.remove(position);
        }
        bucket.push(Entry {
            id,
            address,
            seen: Instant::now(),
        });
    }

    /// Up to `count` entries nearest `target` by XOR distance. Empty rows are
    /// skipped, so a barely-bootstrapped table returns just the seeds it has.
    fn closest(&self, target: &NodeId, count: usize) -> Vec<(NodeId, SocketAddr)> {
        let mut all: Vec<&Entry> = self.buckets.iter().flatten().collect();
        all.sort_by_key(|entry| xor_key(&entry.id, target));
        all.truncate(count);
        all.into_iter()
            .map(|entry| (entry.id, entry.address))
            .collect()
    }

    fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }
}

/// Position of the first differing bit versus our own id.
fn bucket_index(own: &NodeId, node: &NodeId) -> usize {
    let distance = xor_key(own, node);
    for (index, value) in distance.iter().enumerate() {
        if *value != 0 {
            return index * 8 + value.leading_zeros() as usize;
        }
    }
    159
}

// ------------------------------------------------------------ wire frames ---

#[derive(Debug)]
struct Datagram {
    transaction: Vec<u8>,
    content: Content,
}

#[derive(Debug)]
enum Content {
    Response {
        sender: Option<NodeId>,
        body: Body,
    },
    Error {
        code: i64,
        message: String,
    },
    /// Parsed so the wire format stays complete, and read by the test
    /// harnesses; a client-only node otherwise ignores inbound queries.
    #[allow(dead_code)]
    Query(Query),
}

#[derive(Debug)]
enum Body {
    Ack,
    Nodes(Vec<(NodeId, SocketAddr)>),
    Peers {
        token: Vec<u8>,
        nodes: Vec<(NodeId, SocketAddr)>,
        values: Vec<SocketAddr>,
    },
}

#[derive(Debug)]
enum Query {
    Ping,
    FindNode {
        target: NodeId,
    },
    GetPeers {
        info_hash: NodeId,
    },
    AnnouncePeer {
        info_hash: NodeId,
        port: u16,
        token: Vec<u8>,
    },
}

fn bytes_entry(key: &[u8], value: &[u8]) -> (Vec<u8>, Value) {
    (key.to_vec(), Value::Bytes(value.to_vec()))
}

fn action_name(action: &Query) -> &'static [u8] {
    match action {
        Query::Ping => b"ping",
        Query::FindNode { .. } => b"find_node",
        Query::GetPeers { .. } => b"get_peers",
        Query::AnnouncePeer { .. } => b"announce_peer",
    }
}

fn query_packet(node_id: &NodeId, action: &Query, transaction: &[u8; 2]) -> Vec<u8> {
    let arguments = match action {
        Query::Ping => vec![bytes_entry(b"id", node_id)],
        Query::FindNode { target } => {
            vec![bytes_entry(b"id", node_id), bytes_entry(b"target", target)]
        }
        Query::GetPeers { info_hash } => vec![
            bytes_entry(b"id", node_id),
            bytes_entry(b"info_hash", info_hash),
        ],
        Query::AnnouncePeer {
            info_hash,
            port,
            token,
        } => vec![
            bytes_entry(b"id", node_id),
            bytes_entry(b"info_hash", info_hash),
            (b"port".to_vec(), Value::Int(*port as i64)),
            bytes_entry(b"token", token),
        ],
    };
    encode(&Value::Dict(vec![
        (b"t".to_vec(), Value::Bytes(transaction.as_slice().to_vec())),
        (b"y".to_vec(), Value::Bytes(b"q".to_vec())),
        (b"q".to_vec(), Value::Bytes(action_name(action).to_vec())),
        (b"a".to_vec(), Value::Dict(arguments)),
    ]))
}

/// Build a KRPC response. The test harnesses use it: a client-only node does
/// not service inbound queries yet, so nothing responds in production (see
/// the module docs).
#[cfg(test)]
fn response_packet(
    transaction: &[u8],
    response_id: &NodeId,
    nodes: &[(NodeId, SocketAddr)],
    token: Option<&[u8]>,
) -> Vec<u8> {
    let mut reply = vec![bytes_entry(b"id", response_id)];
    if !nodes.is_empty() {
        reply.push((
            b"nodes".to_vec(),
            Value::Bytes(
                nodes
                    .iter()
                    .flat_map(|(id, address)| compact_node(id, *address))
                    .collect(),
            ),
        ));
    }
    if let Some(token) = token {
        reply.push(bytes_entry(b"token", token));
    }
    encode(&Value::Dict(vec![
        (b"t".to_vec(), Value::Bytes(transaction.to_vec())),
        (b"y".to_vec(), Value::Bytes(b"r".to_vec())),
        (b"r".to_vec(), Value::Dict(reply)),
    ]))
}

/// Error replies are harness-only today; same reason as `response_packet`.
#[cfg(test)]
fn error_packet(transaction: &[u8], code: i64, message: &str) -> Vec<u8> {
    encode(&Value::Dict(vec![
        (b"t".to_vec(), Value::Bytes(transaction.to_vec())),
        (b"y".to_vec(), Value::Bytes(b"e".to_vec())),
        (
            b"e".to_vec(),
            Value::List(vec![
                Value::Int(code),
                Value::Bytes(message.as_bytes().to_vec()),
            ]),
        ),
    ]))
}

fn decode_datagram(data: &[u8]) -> Result<Datagram> {
    let root = crate::bencode::decode(data)?;
    let transaction = root
        .get("t")
        .and_then(Value::as_bytes)
        .map(|bytes| bytes.to_vec())
        .ok_or_else(|| Error::Dht("datagram without a transaction id".into()))?;
    let kind = root
        .get("y")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Dht("datagram without a y".into()))?;

    let content = match kind {
        "r" => {
            let body = root
                .get("r")
                .ok_or_else(|| Error::Dht("response without r".into()))?;
            let sender = body
                .get("id")
                .and_then(Value::as_bytes)
                .and_then(|bytes| NodeId::try_from(bytes).ok());
            let nodes = body
                .get("nodes")
                .and_then(Value::as_bytes)
                .map(parse_compact_nodes)
                .unwrap_or_default();
            let token = body
                .get("token")
                .and_then(Value::as_bytes)
                .map(|bytes| bytes.to_vec());
            if let Some(token) = token {
                // A token rides every get_peers reply, holder or not — keep it
                // for the later announce_peer, peers or no peers. The closest
                // nodes for a torrent usually do not hold values themselves.
                let peers: Vec<SocketAddr> = body
                    .get("values")
                    .and_then(Value::as_list)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_bytes)
                            .filter(|bytes| bytes.len() == 6)
                            .map(parse_compact_peer)
                            .collect()
                    })
                    .unwrap_or_default();
                Content::Response {
                    sender,
                    body: Body::Peers {
                        token,
                        nodes,
                        values: peers,
                    },
                }
            } else if nodes.is_empty() {
                Content::Response {
                    sender,
                    body: Body::Ack,
                }
            } else {
                Content::Response {
                    sender,
                    body: Body::Nodes(nodes),
                }
            }
        }
        "e" => {
            let error = root
                .get("e")
                .and_then(Value::as_list)
                .ok_or_else(|| Error::Dht("error datagram without e".into()))?;
            Content::Error {
                code: error.first().and_then(Value::as_int).unwrap_or(0),
                message: error
                    .get(1)
                    .and_then(Value::as_str)
                    .unwrap_or("(unparsable)")
                    .to_string(),
            }
        }
        "q" => {
            let action = root
                .get("q")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Dht("query without a q".into()))?;
            let arguments = root
                .get("a")
                .ok_or_else(|| Error::Dht("query without arguments".into()))?;
            let field = |key: &'static str| -> Result<NodeId> {
                arguments
                    .get(key)
                    .and_then(Value::as_bytes)
                    .and_then(|bytes| NodeId::try_from(bytes).ok())
                    .ok_or_else(|| Error::Dht(format!("{action} has no valid {key}")))
            };
            Content::Query(match action {
                "ping" => Query::Ping,
                "find_node" => Query::FindNode {
                    target: field("target")?,
                },
                "get_peers" => Query::GetPeers {
                    info_hash: field("info_hash")?,
                },
                "announce_peer" => Query::AnnouncePeer {
                    info_hash: field("info_hash")?,
                    port: arguments
                        .get("port")
                        .and_then(Value::as_int)
                        .and_then(|value| u16::try_from(value).ok())
                        .ok_or_else(|| Error::Dht("announce_peer has no port".into()))?,
                    token: arguments
                        .get("token")
                        .and_then(Value::as_bytes)
                        .map(|bytes| bytes.to_vec())
                        .unwrap_or_default(),
                },
                other => return Err(Error::Dht(format!("unknown query {other:?}"))),
            })
        }
        other => return Err(Error::Dht(format!("unknown frame kind {other:?}"))),
    };

    Ok(Datagram {
        transaction,
        content,
    })
}

/// 26-byte compact node: 20-byte id + 4-byte IPv4 + 2-byte port.
#[cfg(test)]
fn compact_node(id: &NodeId, address: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(26);
    out.extend_from_slice(id);
    if let IpAddr::V4(ip) = address.ip() {
        out.extend_from_slice(&ip.octets());
    }
    out.extend_from_slice(&address.port().to_be_bytes());
    out
}

fn parse_compact_nodes(bytes: &[u8]) -> Vec<(NodeId, SocketAddr)> {
    bytes
        .as_chunks::<26>()
        .0
        .iter()
        .map(|chunk| {
            let mut id = [0u8; 20];
            id.copy_from_slice(&chunk[..20]);
            let ip = std::net::Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
            let port = u16::from_be_bytes([chunk[24], chunk[25]]);
            (id, SocketAddr::new(IpAddr::V4(ip), port))
        })
        .collect()
}

/// 6-byte compact peer (a `values` entry) to a socket address (IPv4 only).
fn parse_compact_peer(bytes: &[u8]) -> SocketAddr {
    let chunk: [u8; 6] = bytes.try_into().expect("tested 6-byte slice");
    let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
    SocketAddr::from((ip, u16::from_be_bytes([chunk[4], chunk[5]])))
}

// ------------------------------------------------------------ query node -----

struct Request {
    transaction: u16,
    to: SocketAddr,
    at: Instant,
}

/// A live DHT client: routes, hunts, announces. Inbound queries are not
/// serviced (documented above), so every method runs on the caller's thread.
pub struct Dht {
    id: NodeId,
    socket: UdpSocket,
    routing: Mutex<RoutingTable>,
    tokens: Mutex<HashMap<NodeId, Vec<u8>>>,
    outstanding: Mutex<Vec<Request>>,
}

impl Dht {
    fn bind(port: u16, id: NodeId) -> Result<Dht> {
        let address = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port);
        let socket = UdpSocket::bind(address)
            .map_err(|err| Error::Dht(format!("cannot bind port {port}: {err}")))?;
        socket
            .set_read_timeout(Some(REPLY_WAIT))
            .map_err(|err| Error::Dht(format!("cannot set socket read timeout: {err}")))?;
        Ok(Dht {
            id,
            socket,
            routing: Mutex::new(RoutingTable::new()),
            tokens: Mutex::new(HashMap::new()),
            outstanding: Mutex::new(Vec::new()),
        })
    }

    /// Create, bind, and bootstrap against the standard public routers.
    pub fn boot(node_id: NodeId, port: u16) -> Result<Dht> {
        let routers: Vec<SocketAddr> = DEFAULT_ROUTERS
            .iter()
            .filter_map(|name| {
                name.to_socket_addrs()
                    .ok()
                    .and_then(|mut addresses| addresses.next())
            })
            .collect();
        let dht = Dht::bind(port, node_id)?;
        dht.search(&node_id, &routers, false, BOOTSTRAP_BUDGET);
        Ok(dht)
    }

    /// Local/test constructor: seed directly with addresses (no DNS).
    #[cfg(test)]
    fn start_local(seeds: &[SocketAddr], budget: Duration) -> Result<Dht> {
        let id = random_node_id();
        let dht = Dht::bind(0, id)?;
        dht.search(&id, seeds, false, budget);
        Ok(dht)
    }

    pub fn table_size(&self) -> usize {
        self.routing.lock().unwrap().len()
    }

    /// Send one query, remembering the transaction so its reply can be matched.
    fn ask(&self, action: &Query, address: SocketAddr, transaction: u16) -> Result<()> {
        let packet = query_packet(&self.id, action, &transaction.to_be_bytes());
        self.socket
            .send_to(&packet, address)
            .map_err(|err| Error::Dht(format!("send to {address}: {err}")))?;
        self.outstanding.lock().unwrap().push(Request {
            transaction,
            to: address,
            at: Instant::now(),
        });
        Ok(())
    }

    /// The synchronous search: round-based ALPHA fan-out toward the XOR-closest
    /// unqueried nodes; replies matched by transaction id. Bootstrap `seeds`
    /// are always fair game, and their replies populate the routing table.
    fn search(
        &self,
        target: &NodeId,
        seeds: &[SocketAddr],
        want_peers: bool,
        budget: Duration,
    ) -> Vec<SocketAddr> {
        let deadline = Instant::now() + budget;
        let mut queried: HashSet<(IpAddr, u16)> = HashSet::new();
        let mut peers: Vec<SocketAddr> = Vec::new();

        while Instant::now() < deadline {
            let candidates: Vec<SocketAddr> = self.round(target, &queried, seeds);
            if candidates.is_empty() && self.outstanding.lock().unwrap().is_empty() {
                break; // nothing to ask, nothing in flight: table is static
            }

            for address in candidates {
                let transaction: u16 = rand::thread_rng().gen();
                let action = if want_peers {
                    Query::GetPeers { info_hash: *target }
                } else {
                    Query::FindNode { target: *target }
                };
                if self.ask(&action, address, transaction).is_err() {
                    queried.insert((address.ip(), address.port()));
                }
            }

            // Drain replies until the round's worth is settled or we found
            // enough; the socket's REPLY_WAIT bounds each blocking recv.
            while Instant::now() < deadline {
                self.sweep(&mut queried);
                match self.collect_one(&mut queried, &mut peers, want_peers) {
                    Ok(()) => {}
                    Err(_) => break,
                }
                if want_peers && !peers.is_empty() {
                    break;
                }
                if self.outstanding.lock().unwrap().is_empty() {
                    break;
                }
            }

            if want_peers && !peers.is_empty() {
                break;
            }
        }

        self.outstanding.lock().unwrap().clear();
        peers
    }

    /// ALPHA candidate addresses for the next fan-out round: routing entries
    /// first (XOR-closest), then unfired seeds. Nothing is marked here.
    fn round(
        &self,
        target: &NodeId,
        queried: &HashSet<(IpAddr, u16)>,
        seeds: &[SocketAddr],
    ) -> Vec<SocketAddr> {
        let mut candidates = self
            .routing
            .lock()
            .unwrap()
            .closest(target, 3 * BUCKET_SIZE)
            .into_iter()
            .filter(|(_, address)| !queried.contains(&(address.ip(), address.port())))
            .map(|(_, address)| address)
            .collect::<Vec<_>>();
        for seed in seeds {
            if !queried.contains(&(seed.ip(), seed.port())) {
                candidates.push(*seed);
            }
        }
        candidates.truncate(ALPHA);
        candidates
    }

    /// Drop outstanding entries that have outrun their patience; the address
    /// counts as queried so later rounds don't re-ask dead nodes.
    fn sweep(&self, queried: &mut HashSet<(IpAddr, u16)>) {
        self.outstanding.lock().unwrap().retain(|request| {
            if request.at.elapsed() >= STALE_AFTER {
                queried.insert((request.to.ip(), request.to.port()));
                return false;
            }
            true
        });
    }

    /// Read one datagram. If it completes a known transaction, its nodes/
    /// tokens/peers land in the shared state and the transaction id is
    /// returned. Timeouts and noise are Ok(()).
    fn collect_one(
        &self,
        queried: &mut HashSet<(IpAddr, u16)>,
        peers: &mut Vec<SocketAddr>,
        want_peers: bool,
    ) -> Result<()> {
        let mut buffer = vec![0u8; MAX_PACKET];
        let Ok((length, from)) = self.socket.recv_from(&mut buffer) else {
            return Ok(()); // timeout: the sweep decides whether retries remain
        };
        let Ok(datagram) = decode_datagram(&buffer[..length]) else {
            return Ok(()); // not ours / corrupt: noise
        };
        let Some(frame) = datagram.transaction.as_chunks::<2>().0.first() else {
            return Ok(());
        };
        let transaction = u16::from_be_bytes(*frame);

        let known = self
            .outstanding
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.transaction == transaction);
        if !known {
            return Ok(()); // stale reply from a previous search
        }
        self.outstanding
            .lock()
            .unwrap()
            .retain(|request| request.transaction != transaction);
        queried.insert((from.ip(), from.port()));

        match datagram.content {
            Content::Response { sender, body } => {
                if let Some(sender) = sender {
                    self.routing.lock().unwrap().upsert(&self.id, sender, from);
                }
                match body {
                    Body::Peers {
                        token,
                        nodes,
                        values,
                    } => {
                        if let Some(sender) = sender {
                            self.tokens.lock().unwrap().insert(sender, token);
                        }
                        for (node, address) in nodes {
                            self.routing.lock().unwrap().upsert(&self.id, node, address);
                        }
                        if want_peers {
                            peers.extend(values);
                        }
                    }
                    Body::Nodes(nodes) => {
                        for (node, address) in nodes {
                            self.routing.lock().unwrap().upsert(&self.id, node, address);
                        }
                    }
                    Body::Ack => {}
                }
                Ok(())
            }
            Content::Error { code, message } => Err(Error::Dht(format!(
                "node {from} answered {code}: {message}"
            ))),
            Content::Query(_) => Ok(()), // we do not service inbound queries
        }
    }

    /// Discover peers for `info_hash`, bounded by `want` and `budget`.
    pub fn peer_hunt(&self, info_hash: NodeId, want: usize, budget: Duration) -> Vec<SocketAddr> {
        let mut peers = self.search(
            &info_hash,
            &[],
            true,
            budget.max(Duration::from_millis(500)),
        );
        peers.sort();
        peers.dedup();
        peers.truncate(want.max(1));
        peers
    }

    /// Announce oneself to the token-bearing nodes nearest the torrent.
    /// Best effort: one try per node, ack awaited briefly; acked count returned.
    pub fn announce(&self, info_hash: NodeId, port: u16) -> usize {
        let holders: Vec<(NodeId, SocketAddr)> = {
            let tokens = self.tokens.lock().unwrap();
            self.routing
                .lock()
                .unwrap()
                .closest(&info_hash, BUCKET_SIZE * 2)
                .into_iter()
                .filter(|(node, _)| tokens.contains_key(node))
                .collect()
        };
        let deadline = Instant::now() + ANNOUNCE_BUDGET;
        let mut acknowledged = 0;
        for (node, address) in holders {
            if Instant::now() >= deadline {
                break;
            }
            let Some(token) = self.tokens.lock().unwrap().get(&node).cloned() else {
                continue;
            };
            let transaction: u16 = rand::thread_rng().gen();
            if self
                .ask(
                    &Query::AnnouncePeer {
                        info_hash,
                        port,
                        token,
                    },
                    address,
                    transaction,
                )
                .is_err()
            {
                continue;
            }
            if self.await_ack(transaction, deadline) {
                acknowledged += 1;
            }
        }
        acknowledged
    }

    /// Wait for the announce ack; a reply for a *different* outstanding
    /// transaction is still consumed (routing stays warm), but the answer is
    /// true only for the one asked.
    fn await_ack(&self, transaction: u16, deadline: Instant) -> bool {
        let mut queried: HashSet<(IpAddr, u16)> = HashSet::new();
        let mut await_peers: Vec<SocketAddr> = Vec::new();
        while Instant::now() < deadline {
            let settled = !self
                .outstanding
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.transaction == transaction);
            if settled {
                return true;
            }
            match self.collect_one(&mut queried, &mut await_peers, false) {
                Ok(()) => {}
                Err(_) => return false,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Test peers mint tokens with the same scheme a real node would use:
    /// SHA-1 over (node id, requester ip), first 8 bytes. This engine only
    /// stores and replays tokens — it never answers get_peers itself.
    fn token_for(own: &NodeId, requester: &SocketAddr) -> Vec<u8> {
        let mut hasher = Sha1::new();
        hasher.update(own);
        hasher.update(requester.ip().to_string().as_bytes());
        hasher.finalize()[..8].to_vec()
    }

    /// Values-entry encoding for the fake holder's replies.
    fn compact_peer(address: SocketAddr) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        if let IpAddr::V4(ip) = address.ip() {
            out.extend_from_slice(&ip.octets());
        }
        out.extend_from_slice(&address.port().to_be_bytes());
        out
    }

    fn node_at(distance: u8, port: u16) -> (NodeId, SocketAddr) {
        let mut id = [0u8; 20];
        id[19] = distance;
        (id, SocketAddr::from(([192, 168, 0, 1], port)))
    }

    #[test]
    fn compact_nodes_round_trip_ignoring_a_trailing_partial() {
        let node = ([9u8; 20], SocketAddr::from(([10, 0, 0, 1], 6881)));
        let bytes = compact_node(&node.0, node.1);
        assert_eq!(bytes.len(), 26);
        assert_eq!(parse_compact_nodes(&bytes), vec![node]);
        // A trailing partial node is truncated away, not an error (BEP 5).
        let mut trailing = bytes;
        trailing.push(9);
        assert_eq!(parse_compact_nodes(&trailing), vec![node]);
    }

    #[test]
    fn routing_table_orders_by_xor_distance_and_caps_buckets() {
        let own = [0u8; 20];
        let mut table = RoutingTable::new();
        // Distinct first-differing-bit positions: no eviction, pure ordering.
        for distance in [1u8, 2, 4, 8] {
            let (id, address) = node_at(distance, 1000 + distance as u16);
            table.upsert(&own, id, address);
        }
        let closest = table.closest(&[0u8; 20], 4);
        let distances: Vec<u8> = closest.iter().map(|(id, _)| id[19]).collect();
        assert_eq!(distances, vec![1, 2, 4, 8], "nearest first");

        // Nine ids whose distance shares leading_zeros (16..=24 → bucket 155):
        // the bucket caps at BUCKET_SIZE and the stalest entry leaves.
        for distance in 16u8..=23 {
            let (id, address) = node_at(distance, 2000 + distance as u16);
            table.upsert(&own, id, address);
        }
        assert_eq!(table.buckets[155].len(), 8);
        let (id_24, address_24) = node_at(24, 8124);
        table.upsert(&own, id_24, address_24);
        assert_eq!(table.buckets[155].len(), 8, "full bucket evicts, not grows");
        assert!(table.buckets[155].iter().any(|entry| entry.id[19] == 24));
        assert!(
            table.buckets[155].iter().all(|entry| entry.id[19] != 16),
            "the stalest entry was evicted"
        );

        // Re-seen ids refresh in place rather than evicting anyone else.
        let (id_17, _) = node_at(17, 2017);
        table.upsert(&own, id_17, SocketAddr::from(([10, 0, 0, 17], 9999)));
        assert_eq!(table.buckets[155].len(), 8);
        let refreshed = table.buckets[155]
            .iter()
            .find(|entry| entry.id == id_17)
            .unwrap();
        assert_eq!(refreshed.address, SocketAddr::from(([10, 0, 0, 17], 9999)));
    }

    #[test]
    fn queries_round_trip_through_the_codec() {
        for action in [
            Query::Ping,
            Query::FindNode { target: [2u8; 20] },
            Query::GetPeers {
                info_hash: [3u8; 20],
            },
            Query::AnnouncePeer {
                info_hash: [4u8; 20],
                port: 6881,
                token: b"tok".to_vec(),
            },
        ] {
            let packet = query_packet(&[9u8; 20], &action, &[1, 2]);
            let datagram = decode_datagram(&packet).unwrap();
            assert_eq!(datagram.transaction, vec![1, 2], "{action:?}");
            match (action, datagram.content) {
                (Query::Ping, Content::Query(Query::Ping)) => {}
                (Query::FindNode { target: a }, Content::Query(Query::FindNode { target: b })) => {
                    assert_eq!(a, b)
                }
                (
                    Query::GetPeers { info_hash: a },
                    Content::Query(Query::GetPeers { info_hash: b }),
                ) => {
                    assert_eq!(a, b)
                }
                (
                    Query::AnnouncePeer {
                        info_hash: a,
                        port: a_port,
                        token: a_token,
                    },
                    Content::Query(Query::AnnouncePeer {
                        info_hash: b,
                        port: b_port,
                        token: b_token,
                    }),
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(a_port, b_port);
                    assert_eq!(a_token, b_token);
                }
                (action, other) => panic!("decode mismatch: {action:?} → {other:?}"),
            }
        }
    }

    #[test]
    fn error_packets_decode_with_code_and_message() {
        let datagram = decode_datagram(&error_packet(&[0, 1], 203, "invalid token")).unwrap();
        match datagram.content {
            Content::Error { code, message } => {
                assert_eq!((code, message.as_str()), (203, "invalid token"));
            }
            other => panic!("an error packet decoded as {other:?}"),
        }
    }

    #[test]
    fn tokens_bind_to_the_requester_address() {
        let own = [1u8; 20];
        let local = SocketAddr::from(([127, 0, 0, 1], 6881));
        assert_eq!(token_for(&own, &local), token_for(&own, &local));
        let other = SocketAddr::from(([127, 0, 0, 2], 6881));
        assert_ne!(token_for(&own, &local), token_for(&own, &other));
    }

    /// A stand-in public router: acks pings, answers node lookups with one
    /// steering entry, and always hands a get_peers token.
    fn fake_router(socket: UdpSocket, stop: Arc<AtomicBool>, peer: SocketAddr, peer_id: NodeId) {
        let id = [1u8; 20];
        let mut buffer = vec![0u8; MAX_PACKET];
        let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
        while !stop.load(Ordering::Relaxed) {
            let Ok((length, from)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            let Ok(datagram) = decode_datagram(&buffer[..length]) else {
                continue;
            };
            let reply = match datagram.content {
                Content::Query(Query::Ping) | Content::Query(Query::AnnouncePeer { .. }) => {
                    response_packet(&datagram.transaction, &id, &[], None)
                }
                Content::Query(Query::FindNode { .. }) => {
                    response_packet(&datagram.transaction, &id, &[(peer_id, peer)], None)
                }
                Content::Query(Query::GetPeers { .. }) => response_packet(
                    &datagram.transaction,
                    &id,
                    &[(peer_id, peer)],
                    Some(&token_for(&id, &from)),
                ),
                _ => continue,
            };
            let _ = socket.send_to(&reply, from);
        }
    }

    /// The swarm-holder node: reports one peer for every get_peers and
    /// validates the announce token (203 on a wrong one), recording the
    /// announced port.
    fn fake_peer(
        socket: UdpSocket,
        stop: Arc<AtomicBool>,
        peer_port: u16,
        announced: Arc<Mutex<Option<u16>>>,
    ) {
        let id = [2u8; 20];
        let mut buffer = vec![0u8; MAX_PACKET];
        let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
        while !stop.load(Ordering::Relaxed) {
            let Ok((length, from)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            let Ok(datagram) = decode_datagram(&buffer[..length]) else {
                continue;
            };
            let expected_token = token_for(&id, &from);
            match datagram.content {
                Content::Query(Query::GetPeers { .. }) => {
                    let reply = encode(&Value::Dict(vec![
                        (b"t".to_vec(), Value::Bytes(datagram.transaction)),
                        (b"y".to_vec(), Value::Bytes(b"r".to_vec())),
                        (
                            b"r".to_vec(),
                            Value::Dict(vec![
                                (b"id".to_vec(), Value::Bytes(id.to_vec())),
                                (b"token".to_vec(), Value::Bytes(expected_token)),
                                (
                                    b"values".to_vec(),
                                    Value::List(vec![Value::Bytes(compact_peer(
                                        SocketAddr::from(([127, 0, 0, 1], peer_port)),
                                    ))]),
                                ),
                            ]),
                        ),
                    ]));
                    let _ = socket.send_to(&reply, from);
                }
                Content::Query(Query::AnnouncePeer { port, token, .. }) => {
                    if token == expected_token {
                        *announced.lock().unwrap() = Some(port);
                        let reply = response_packet(&datagram.transaction, &id, &[], None);
                        let _ = socket.send_to(&reply, from);
                    } else {
                        let reply = error_packet(&datagram.transaction, 203, "invalid token");
                        let _ = socket.send_to(&reply, from);
                    }
                }
                Content::Query(_) => {
                    let reply = response_packet(&datagram.transaction, &id, &[], None);
                    let _ = socket.send_to(&reply, from);
                }
                _ => {}
            }
        }
    }

    /// Router steers us to the peer node; the peer node hands the torrent's
    /// only peer over with a token, and the token lets us announce back.
    #[test]
    fn hunts_peers_through_the_router_chain_then_announces() {
        let info_hash = [5u8; 20];
        let peer_node_id = [2u8; 20];

        // The "peer" is just an address a live node would hand us; a TCP
        // listener guarantees a stable port for the assertion.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let peer_port = listener.local_addr().unwrap().port();

        let router_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let router_addr = router_sock.local_addr().unwrap();
        let peer_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer_sock.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let announced: Arc<Mutex<Option<u16>>> = Arc::new(Mutex::new(None));

        let router_thread = {
            let stop = stop.clone();
            std::thread::spawn(move || fake_router(router_sock, stop, peer_addr, peer_node_id))
        };
        let peer_thread = {
            let stop = stop.clone();
            let announced = announced.clone();
            std::thread::spawn(move || fake_peer(peer_sock, stop, peer_port, announced))
        };

        // Bootstrap: the router's find_node reply introduces the peer node.
        let client =
            Dht::start_local(&[router_addr], Duration::from_secs(3)).expect("client boots");
        assert_eq!(client.table_size(), 2, "router steered us to the peer node");

        let peers = client.peer_hunt(info_hash, 10, Duration::from_secs(3));
        assert_eq!(peers, vec![SocketAddr::from(([127, 0, 0, 1], peer_port))]);

        // Both token-holding nodes acknowledge the announce; the holder
        // validates its token and records the port.
        let acked = client.announce(info_hash, peer_port);
        assert_eq!(acked, 2, "router and holder both acknowledged");
        assert_eq!(
            *announced.lock().unwrap(),
            Some(peer_port),
            "holder validated the announce token"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = router_thread.join();
        let _ = peer_thread.join();
    }
}
