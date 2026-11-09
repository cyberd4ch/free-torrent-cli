//! Download orchestration: connect to peers, fetch pieces, verify, write, resume.

use crate::dht::{self, Dht};
use crate::error::{Error, Result};
use crate::peer::{self, Message, BLOCK_SIZE};
use crate::storage::Storage;
use crate::torrent::Torrent;
use crate::tracker::{self, AnnounceRequest, Event};
use rand::Rng;
use sha1::{Digest, Sha1};
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many DHT peers to fold into one tracker refill.
const DHT_HUNT_WANT: usize = 16;
/// Waiting time per DHT refill inside a tracker cycle.
const DHT_REFILL: Duration = Duration::from_secs(6);

/// How long a worker with an empty address queue keeps faith with the swarm
/// before handing its slot back (run()'s attempt cycle then re-announces).
const PEER_WAIT: Duration = Duration::from_secs(6);
/// Minimum gap between announce bursts that starved workers fire themselves,
/// so a pack of peers waiting for addresses cannot stampede the tracker.
const PEER_REFILL_GAP: Duration = Duration::from_millis(2500);

#[derive(Debug, Clone)]
pub struct Options {
    pub max_peers: usize,
    pub port: u16,
    /// Discover peers from the DHT (BEP 5) in addition to the trackers.
    pub use_dht: bool,
    /// Cap the download at this many bytes per second across every peer.
    /// `None` means unlimited.
    pub limit_bps: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_peers: 6,
            port: 6881,
            use_dht: true,
            limit_bps: None,
        }
    }
}

#[derive(Debug)]
pub struct Progress {
    pub total: u64,
    pub pieces: usize,
    downloaded: AtomicU64,
    verified: AtomicUsize,
    active_peers: AtomicUsize,
    done: AtomicBool,
    started: Instant,
    peers: Mutex<Vec<PeerView>>,
}

/// One connection the swarm offered us, and what it is doing right now.
#[derive(Debug, Clone)]
pub struct PeerView {
    pub address: SocketAddr,
    pub state: &'static str,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub total: u64,
    pub downloaded: u64,
    pub pieces: usize,
    pub verified: usize,
    pub active_peers: usize,
    pub done: bool,
    pub elapsed: Duration,
    pub peers: Vec<PeerView>,
}

impl Progress {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            total: self.total,
            downloaded: self.downloaded.load(Ordering::Relaxed),
            pieces: self.pieces,
            verified: self.verified.load(Ordering::Relaxed),
            active_peers: self.active_peers.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
            elapsed: self.started.elapsed(),
            peers: self.peers.lock().unwrap().clone(),
        }
    }

    /// Record a peer the swarm handed us, so the UI can list the connection
    /// set even before a piece has been requested from it.
    pub fn track_peer(&self, address: SocketAddr) {
        let mut peers = self.peers.lock().unwrap();
        if peers.iter().any(|peer| peer.address == address) {
            return;
        }
        if peers.len() < 200 {
            peers.push(PeerView {
                address,
                state: "idle",
                bytes: 0,
            });
        }
    }

    pub fn mark_peer(&self, address: SocketAddr, state: &'static str) {
        let mut peers = self.peers.lock().unwrap();
        match peers.iter_mut().find(|peer| peer.address == address) {
            Some(peer) => peer.state = state,
            None => peers.push(PeerView {
                address,
                state,
                bytes: 0,
            }),
        }
    }

    pub fn add_peer_bytes(&self, address: SocketAddr, bytes: u64) {
        let mut peers = self.peers.lock().unwrap();
        if let Some(peer) = peers.iter_mut().find(|peer| peer.address == address) {
            peer.bytes += bytes;
        }
    }

    pub fn bytes_per_second(&self) -> f64 {
        let snapshot = self.snapshot();
        let seconds = snapshot.elapsed.as_secs_f64().max(0.001);
        snapshot.downloaded as f64 / seconds
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub downloaded: u64,
    pub pieces: usize,
    pub elapsed: Duration,
    pub resumed: usize,
}

/// A shared token bucket: each worker reserves the time its next block will
/// take, so the *aggregate* rate across all peers stays at `bytes_per_second`
/// rather than each peer getting the full budget.
#[derive(Debug)]
pub struct Throttle {
    bytes_per_second: u64,
    next: Mutex<Instant>,
}

impl Throttle {
    pub fn new(bytes_per_second: u64) -> Throttle {
        Throttle {
            bytes_per_second: bytes_per_second.max(1),
            next: Mutex::new(Instant::now()),
        }
    }

    pub fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second
    }

    /// Wait long enough to keep the long-run average, then return.
    fn reserve(&self, bytes: usize) {
        let wait = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            let slot = (*next).max(now);
            *next = slot + Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

pub struct Session {
    torrent: Arc<Torrent>,
    storage: Arc<Storage>,
    missing: Arc<Mutex<VecDeque<usize>>>,
    peers: Arc<Mutex<VecDeque<SocketAddr>>>,
    progress: Arc<Progress>,
    peer_id: [u8; 20],
    options: Options,
    dht: Option<Dht>,
    resumed: usize,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    throttle: Option<Arc<Throttle>>,
    explicit: Option<Arc<Vec<SocketAddr>>>,
}

impl Session {
    pub fn open(
        torrent: Torrent,
        output: &Path,
        options: Options,
    ) -> Result<(Self, Arc<Progress>)> {
        let storage = Storage::open(output, &torrent)?;
        let have = storage.verify_existing(&torrent)?;
        let missing: VecDeque<usize> = have
            .iter()
            .enumerate()
            .filter(|(_, present)| !**present)
            .map(|(index, _)| index)
            .collect();
        let resumed = torrent.piece_count() - missing.len();
        let torrent = Arc::new(torrent);
        let progress = Arc::new(Progress {
            total: torrent.total_length,
            pieces: torrent.piece_count(),
            downloaded: AtomicU64::new(
                (0..torrent.piece_count())
                    .filter(|index| have[*index])
                    .map(|index| torrent.piece_size(index))
                    .sum(),
            ),
            verified: AtomicUsize::new(resumed),
            active_peers: AtomicUsize::new(0),
            done: AtomicBool::new(missing.is_empty()),
            started: Instant::now(),
            peers: Mutex::new(Vec::new()),
        });
        let throttle = options.limit_bps.map(Throttle::new).map(Arc::new);
        let session = Session {
            torrent,
            storage: Arc::new(storage),
            missing: Arc::new(Mutex::new(missing)),
            peers: Arc::new(Mutex::new(VecDeque::new())),
            progress: progress.clone(),
            peer_id: make_peer_id(),
            options,
            dht: None,
            resumed,
            stop: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            throttle,
            explicit: None,
        };
        Ok((session, progress))
    }

    pub fn torrent(&self) -> &Torrent {
        &self.torrent
    }

    /// A flag that stops the run at its next checkpoint; `run` then returns
    /// [`Error::Cancelled`]. This is how the web UI cancels a job.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// A flag that parks the run in place: no new pieces are requested while it
    /// is set, and clearing it continues from the same queue. This is how the
    /// web UI pauses a job without losing verified progress.
    pub fn pause_handle(&self) -> Arc<AtomicBool> {
        self.pause.clone()
    }

    /// Block while paused, returning `Err(Cancelled)` if a stop arrives meanwhile.
    fn wait_while_paused(&self) -> Result<()> {
        while self.pause.load(Ordering::Relaxed) {
            if self.stop.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Run to completion, using `explicit_peers` when supplied (tests, `--peer`).
    pub fn run(mut self, explicit_peers: Option<Vec<SocketAddr>>) -> Result<Stats> {
        let started = Instant::now();
        let mut attempts = 0;
        let explicit = explicit_peers.map(Arc::new);
        // Unless the caller handed over peers by hand, join the DHT so that
        // discovery does not rely on trackers alone.
        if explicit.is_none() && self.options.use_dht {
            match Dht::boot(dht::random_node_id(), self.options.port) {
                Ok(node) => self.dht = Some(node),
                Err(err) => eprintln!("DHT unavailable, trackers only: {err}"),
            }
        }
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            self.wait_while_paused()?;
            let remaining = self.missing.lock().unwrap().len();
            if remaining == 0 {
                self.progress.done.store(true, Ordering::Relaxed);
                break;
            }
            if attempts >= 4 {
                return Err(Error::Peer(format!(
                    "{remaining} pieces still missing after retries"
                )));
            }
            attempts += 1;

            self.fill_peers(explicit.as_ref().map(|list| list.as_slice()), attempts)?;
            if self.peers.lock().unwrap().is_empty() {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }

            self.explicit = explicit.clone();
            self.spawn_workers();
            if self.stop.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.missing.lock().unwrap().is_empty() {
                break;
            }
            if attempts < 4 {
                std::thread::sleep(Duration::from_secs(2));
            }
        }

        if !self.torrent.trackers.is_empty() {
            let done = self.progress.snapshot();
            let request = AnnounceRequest {
                info_hash: self.torrent.info_hash,
                peer_id: self.peer_id,
                port: self.options.port,
                uploaded: 0,
                downloaded: done.downloaded,
                left: 0,
                event: Event::Completed,
            };
            for url in &self.torrent.trackers {
                let _ = tracker::announce(url, &request);
            }
        }
        // Tell the DHT where to find us now that the download is complete.
        if let Some(node) = &self.dht {
            node.announce(self.torrent.info_hash, self.options.port);
        }

        Ok(Stats {
            downloaded: self.progress.snapshot().downloaded,
            pieces: self.torrent.piece_count(),
            elapsed: started.elapsed(),
            resumed: self.resumed,
        })
    }

    fn fill_peers(&self, explicit: Option<&[SocketAddr]>, attempt: usize) -> Result<()> {
        let mut peers = self.peers.lock().unwrap();
        peers.clear();
        if let Some(list) = explicit {
            peers.extend(list.iter().copied());
            return Ok(());
        }
        if attempt > 1 {
            std::thread::sleep(Duration::from_secs(5));
        }
        let snapshot = self.progress.snapshot();
        let left = snapshot.total.saturating_sub(snapshot.downloaded);
        let request =
            AnnounceRequest::new(self.torrent.info_hash, self.peer_id, snapshot.total, left);
        let mut errors = Vec::new();
        for url in &self.torrent.trackers {
            match tracker::announce(url, &request) {
                Ok(response) => {
                    peers.extend(response.peers.into_iter().filter(|addr| addr.is_ipv4()));
                }
                Err(err) => errors.push(format!("{url}: {err}")),
            }
        }
        // Trackers give the swarm a start; the DHT (if up) pads the list with
        // whichever peers it knows for this torrent.
        if let Some(node) = &self.dht {
            peers.extend(node.peer_hunt(self.torrent.info_hash, DHT_HUNT_WANT, DHT_REFILL));
        }
        let mut seen = std::collections::HashSet::new();
        peers.retain(|address| seen.insert(*address));
        // Show the discovered swarm as soon as it is known, not only once a
        // worker happens to be talking to one of its members.
        for address in peers.iter() {
            self.progress.track_peer(*address);
        }
        if peers.is_empty() && self.torrent.trackers.is_empty() && !errors.is_empty() {
            return Err(Error::Tracker(errors.join("; ")));
        }
        Ok(())
    }

    fn spawn_workers(&self) {
        let mut handles = Vec::new();
        for _ in 0..self.options.max_peers {
            let session = self.shared();
            handles.push(std::thread::spawn(move || session.worker_loop()));
        }
        for handle in handles {
            let _ = handle.join();
        }
    }

    fn shared(&self) -> Shared {
        Shared {
            torrent: self.torrent.clone(),
            storage: self.storage.clone(),
            missing: self.missing.clone(),
            peers: self.peers.clone(),
            progress: self.progress.clone(),
            peer_id: self.peer_id,
            stop: self.stop.clone(),
            pause: self.pause.clone(),
            throttle: self.throttle.clone(),
            explicit: self.explicit.clone(),
            last_refill: Arc::new(Mutex::new(
                Instant::now()
                    .checked_sub(PEER_REFILL_GAP)
                    .unwrap_or_else(Instant::now),
            )),
        }
    }
}

#[derive(Clone)]
struct Shared {
    torrent: Arc<Torrent>,
    storage: Arc<Storage>,
    missing: Arc<Mutex<VecDeque<usize>>>,
    peers: Arc<Mutex<VecDeque<SocketAddr>>>,
    progress: Arc<Progress>,
    peer_id: [u8; 20],
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    throttle: Option<Arc<Throttle>>,
    explicit: Option<Arc<Vec<SocketAddr>>>,
    last_refill: Arc<Mutex<Instant>>,
}

impl Shared {
    /// Park while paused, or bail out if a stop arrived. Called between blocks
    /// so Pause and Stop land within one 16 KiB block instead of at the end of
    /// a 256 KiB piece that can take seconds on a slow peer.
    fn checkpoint(&self) -> Result<()> {
        while self.pause.load(Ordering::Relaxed) {
            if self.stop.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.stop.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(())
    }

    fn worker_loop(&self) {
        // One negotiated connection serves as many pieces as it stays alive;
        // the worker only ends when the pool empties, a stop lands, or every
        // candidate peer refused.
        let mut link: Option<(SocketAddr, TcpStream, Option<Vec<u8>>)> = None;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            // Park here rather than finishing the in-flight piece round: a paused
            // job stops touching the network within one block.
            while self.pause.load(Ordering::Relaxed) {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let piece = match self.missing.lock().unwrap().pop_front() {
                Some(piece) => piece,
                None => return,
            };
            if link.is_none() {
                link = match self.acquire_connection() {
                    Some(link) => Some(link),
                    None => {
                        // The swarm went quiet and stayed quiet: hand the piece
                        // back and let run()'s attempt cycle take over.
                        self.missing.lock().unwrap().push_front(piece);
                        return;
                    }
                };
            }
            let Some((address, stream, bits)) = link.as_mut() else {
                return;
            };
            self.progress.active_peers.fetch_add(1, Ordering::Relaxed);
            let result = self.fetch_piece(stream, bits.as_deref(), piece);
            self.progress.active_peers.fetch_sub(1, Ordering::Relaxed);

            match result {
                Ok(data) => {
                    self.progress.add_peer_bytes(*address, data.len() as u64);
                    self.progress.mark_peer(*address, "idle");
                    match self.commit(piece, &data) {
                        Ok(()) => {}
                        // Disk troubles: the piece, not the peer, waits.
                        Err(err) => {
                            eprintln!("piece {piece} rejected: {err}");
                            self.missing.lock().unwrap().push_back(piece);
                        }
                    }
                }
                Err(err) => {
                    self.progress.mark_peer(*address, "failed");
                    eprintln!("peer {address} failed on piece {piece}: {err}");
                    // Any failure - protocol, corrupt block, hangup - poisons
                    // the connection: a fresh one opens for the next piece.
                    link = None;
                    self.missing.lock().unwrap().push_front(piece);
                }
            }
        }
    }

    /// Handshake and unchoke with one peer, returning the live stream and its
    /// bitfield (when it announced one) for the pieces ahead.
    fn negotiate(&self, address: SocketAddr) -> Result<(TcpStream, Option<Vec<u8>>)> {
        self.progress.mark_peer(address, "connecting");
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        // Requests are small and urgent: let them leave without waiting for
        // acknowledgements to pile up.
        stream.set_nodelay(true)?;
        peer::write_handshake(&mut stream, &self.torrent.info_hash, &self.peer_id)?;
        peer::read_handshake(&mut stream, &self.torrent.info_hash)?;
        peer::write_message(&mut stream, &Message::Interested)?;

        let mut bitfield: Option<Vec<u8>> = None;
        loop {
            match peer::read_message(&mut stream)? {
                Message::Bitfield(bits) => bitfield = Some(bits),
                Message::Unchoke => break,
                Message::Choke => {}
                // Real clients answer our handshake with their BEP 10 extension
                // handshake (and sometimes a port message) before unchoking;
                // those are routine traffic, not a reason to drop the peer.
                Message::KeepAlive
                | Message::Have(_)
                | Message::Port(_)
                | Message::Extended(..) => {}
                other => {
                    return Err(Error::Peer(format!(
                        "expected unchoke, got message id {}",
                        other.id()
                    )))
                }
            }
        }
        Ok((stream, bitfield))
    }

    /// Wait (bounded) for the swarm to yield a peer worth negotiating with.
    /// The address queue is refilled by run()'s attempt cycle and, when that
    /// is too slow for us, by our own tracker burst. `None` means give the
    /// worker slot back to the session.
    fn acquire_connection(&self) -> Option<(SocketAddr, TcpStream, Option<Vec<u8>>)> {
        let deadline = Instant::now() + PEER_WAIT;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return None;
            }
            while self.pause.load(Ordering::Relaxed) {
                if self.stop.load(Ordering::Relaxed) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let address = {
                let mut queue = self.peers.lock().unwrap();
                let address = queue.pop_front();
                // Small swarms circulate: one address may serve several
                // connections, so the queue never drains below a few entries
                // and a lone seeder cannot strand the round.
                if let Some(address) = &address {
                    if queue.len() < 2 {
                        queue.push_back(*address);
                    }
                }
                address
            };
            if let Some(address) = address {
                match self.negotiate(address) {
                    Ok((stream, bits)) => return Some((address, stream, bits)),
                    Err(err) => {
                        eprintln!("peer {address} refused: {err}");
                        self.progress.mark_peer(address, "failed");
                        continue;
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.refill_peers();
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// One tracker burst that replaces the address queue. Starved workers fire
    /// this so a round does not die waiting for run()'s next attempt; the gap
    /// keeps a pack of hungry workers from stampeding the tracker at once.
    /// DHT (the slow half of discovery) stays with run()'s attempt cycle.
    fn refill_peers(&self) {
        {
            let mut last = self.last_refill.lock().unwrap();
            if last.elapsed() < PEER_REFILL_GAP {
                return;
            }
            *last = Instant::now();
        }
        let snapshot = self.progress.snapshot();
        let left = snapshot.total.saturating_sub(snapshot.downloaded);
        let request =
            AnnounceRequest::new(self.torrent.info_hash, self.peer_id, snapshot.total, left);
        let mut discovered = Vec::new();
        for url in &self.torrent.trackers {
            if let Ok(response) = tracker::announce(url, &request) {
                discovered.extend(response.peers.into_iter().filter(|addr| addr.is_ipv4()));
            }
        }
        let mut list: Vec<SocketAddr> = self
            .explicit
            .as_ref()
            .map(|list| list.iter().copied().collect())
            .unwrap_or(discovered);
        let mut seen = std::collections::HashSet::new();
        list.retain(|address| seen.insert(*address));
        {
            let mut queue = self.peers.lock().unwrap();
            queue.clear();
            queue.extend(list.iter().copied());
        }
        for address in &list {
            self.progress.track_peer(*address);
        }
    }

    fn commit(&self, index: usize, data: &[u8]) -> Result<()> {
        // fetch_piece already proved the SHA-1; here the piece lands.
        self.storage
            .write_piece(index, self.torrent.piece_length, data)?;
        self.progress
            .downloaded
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.progress.verified.fetch_add(1, Ordering::Relaxed);
        let snapshot = self.progress.snapshot();
        if snapshot.verified == snapshot.pieces {
            self.progress.done.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn fetch_piece(
        &self,
        mut stream: &mut TcpStream,
        bits: Option<&[u8]>,
        index: usize,
    ) -> Result<Vec<u8>> {
        if let Some(bits) = bits {
            if !peer::has_piece(bits, index) {
                return Err(Error::Peer(format!("peer does not have piece {index}")));
            }
        }

        let mut piece = vec![0u8; self.torrent.piece_size(index) as usize];
        for (begin, length) in peer::pick_blocks(piece.len() as u64, BLOCK_SIZE) {
            self.checkpoint()?;
            // Pay the wait before asking for the block, so a fast peer cannot
            // race ahead of the cap.
            if let Some(throttle) = &self.throttle {
                throttle.reserve(length as usize);
            }
            peer::write_message(
                &mut stream,
                &Message::Request {
                    index: index as u32,
                    begin,
                    length,
                },
            )?;
            loop {
                match peer::read_message(&mut stream)? {
                    Message::Piece {
                        index: received,
                        begin: at,
                        data,
                    } => {
                        if received as usize != index || at != begin {
                            return Err(Error::Peer("unexpected block in piece response".into()));
                        }
                        if data.len() != length as usize {
                            return Err(Error::Peer(format!(
                                "block is {} bytes, expected {length}",
                                data.len()
                            )));
                        }
                        piece[begin as usize..begin as usize + data.len()].copy_from_slice(&data);
                        break;
                    }
                    Message::Choke => {
                        return Err(Error::Peer("peer choked mid-piece".into()));
                    }
                    Message::KeepAlive
                    | Message::Have(_)
                    | Message::Unchoke
                    | Message::Port(_)
                    | Message::Extended(..) => {}
                    other => {
                        return Err(Error::Peer(format!(
                            "expected block, got message id {}",
                            other.id()
                        )))
                    }
                }
            }
        }
        // Corrupt bytes fail here, so the caller disconnects the peer that
        // sent them instead of writing the damage to disk.
        let mut hasher = Sha1::new();
        hasher.update(&piece);
        if hasher.finalize().as_slice() != self.torrent.pieces[index] {
            return Err(Error::Peer(format!(
                "piece {index} failed SHA-1 verification"
            )));
        }
        Ok(piece)
    }
}

pub fn make_peer_id() -> [u8; 20] {
    let mut rng = rand::thread_rng();
    let mut id = [0u8; 20];
    let prefix = b"-FT0300-";
    id[..prefix.len()].copy_from_slice(prefix);
    for slot in id.iter_mut().skip(prefix.len()) {
        *slot = b"0123456789abcdefghijklmnopqrstuvwxyz"[rng.gen_range(0..36)];
    }
    id
}

/// Resolve a magnet to a torrent by asking an HTTP tracker for the metadata peers.
pub fn output_path(output: &Path, name: &str) -> PathBuf {
    output.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::{self, Message};
    use crate::torrent::tests::single_file_torrent;
    use std::net::TcpListener;

    /// A seeder that reports every block request and holds its first reply until
    /// the test opens the gate, so a test can flip pause/stop at a block edge.
    fn fake_peer_gated(
        listener: TcpListener,
        data: Vec<u8>,
        piece_length: usize,
        pieces: usize,
        info_hash: [u8; 20],
        requests: std::sync::mpsc::Sender<()>,
        first_gate: std::sync::mpsc::Receiver<()>,
    ) {
        let mut first = true;
        // One connection, served until the leech hangs up: the piece worker
        // keeps a negotiated peer for the whole round.
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        if peer::read_handshake(&mut stream, &info_hash).is_err() {
            return;
        }
        let _ = peer::write_handshake(&mut stream, &info_hash, &[7u8; 20]);
        let mut bits = vec![0u8; pieces.div_ceil(8)];
        for index in 0..pieces {
            bits[index / 8] |= 0x80 >> (index % 8);
        }
        let _ = peer::write_message(
            &mut stream,
            &Message::Extended(0, b"d1:md11:ut_metadatai1:ee".to_vec()),
        );
        let _ = peer::write_message(&mut stream, &Message::Bitfield(bits));
        while !matches!(peer::read_message(&mut stream), Ok(Message::Interested)) {}
        let _ = peer::write_message(&mut stream, &Message::Unchoke);
        while let Ok(message) = peer::read_message(&mut stream) {
            if let Message::Request {
                index,
                begin,
                length,
            } = message
            {
                let _ = requests.send(());
                if first {
                    first = false;
                    let _ = first_gate.recv();
                }
                let start = index as usize * piece_length + begin as usize;
                let end = (start + length as usize).min(data.len());
                let _ = peer::write_message(
                    &mut stream,
                    &Message::Piece {
                        index,
                        begin,
                        data: data[start..end].to_vec(),
                    },
                );
            }
        }
    }

    /// A minimal seeder: handshake, full bitfield, unchoke, then answer every request.
    fn fake_peer(
        listener: TcpListener,
        data: Vec<u8>,
        piece_length: usize,
        pieces: usize,
        info_hash: [u8; 20],
    ) {
        // One connection, served until the leech hangs up: the piece worker
        // keeps a negotiated peer for the whole round.
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        if peer::read_handshake(&mut stream, &info_hash).is_err() {
            return;
        }
        let _ = peer::write_handshake(&mut stream, &info_hash, &[7u8; 20]);
        let mut bits = vec![0u8; pieces.div_ceil(8)];
        for index in 0..pieces {
            bits[index / 8] |= 0x80 >> (index % 8);
        }
        // A BEP 10 peer sends its extension handshake right after the wire
        // handshake, before the bitfield and unchoke; the piece worker must
        // ignore it instead of calling the peer broken.
        let _ = peer::write_message(
            &mut stream,
            &Message::Extended(0, b"d1:md11:ut_metadatai1:ee".to_vec()),
        );
        let _ = peer::write_message(&mut stream, &Message::Bitfield(bits));
        while !matches!(peer::read_message(&mut stream), Ok(Message::Interested)) {}
        let _ = peer::write_message(&mut stream, &Message::Unchoke);
        while let Ok(message) = peer::read_message(&mut stream) {
            if let Message::Request {
                index,
                begin,
                length,
            } = message
            {
                let start = index as usize * piece_length + begin as usize;
                let end = (start + length as usize).min(data.len());
                let _ = peer::write_message(
                    &mut stream,
                    &Message::Piece {
                        index,
                        begin,
                        data: data[start..end].to_vec(),
                    },
                );
            }
        }
    }

    #[test]
    fn downloads_verifies_and_writes_a_torrent() {
        let data: Vec<u8> = (0..40_000u32).map(|index| (index % 251) as u8).collect();
        let piece_length = 16_384usize;
        let raw = single_file_torrent("payload.bin", &data, piece_length);
        let torrent = Torrent::parse(&raw).unwrap();
        let pieces = torrent.piece_count();
        let info_hash = torrent.info_hash;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            std::thread::spawn(move || fake_peer(listener, data, piece_length, pieces, info_hash));

        let dir = std::env::temp_dir().join(format!("ftc-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, progress) = Session::open(
            torrent,
            &dir,
            Options {
                max_peers: 1,
                port: 6881,
                use_dht: false,
                limit_bps: None,
            },
        )
        .unwrap();
        let stats = session.run(Some(vec![address])).unwrap();

        assert_eq!(stats.downloaded, 40_000);
        assert_eq!(stats.resumed, 0);
        assert!(progress.snapshot().done);
        let written = std::fs::read(dir.join("payload.bin")).unwrap();
        assert_eq!(written.len(), 40_000);

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_paused_run_does_not_finish_until_it_is_resumed() {
        let data: Vec<u8> = (0..40_000u32).map(|index| (index % 251) as u8).collect();
        let piece_length = 16_384usize;
        let raw = single_file_torrent("paused.bin", &data, piece_length);
        let torrent = Torrent::parse(&raw).unwrap();
        let pieces = torrent.piece_count();
        let info_hash = torrent.info_hash;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            std::thread::spawn(move || fake_peer(listener, data, piece_length, pieces, info_hash));

        let dir = std::env::temp_dir().join(format!("ftc-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, _progress) = Session::open(
            torrent,
            &dir,
            Options {
                max_peers: 1,
                port: 6881,
                use_dht: false,
                limit_bps: None,
            },
        )
        .unwrap();

        let pause = session.pause_handle();
        pause.store(true, Ordering::Relaxed);
        let gate = pause.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            gate.store(false, Ordering::Relaxed);
        });

        let stats = session.run(Some(vec![address])).unwrap();
        assert_eq!(stats.downloaded, 40_000);
        assert!(
            stats.elapsed >= Duration::from_millis(400),
            "the run finished in {:?}, so the pause never blocked it",
            stats.elapsed
        );

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pause_lands_between_blocks_not_at_the_end_of_a_piece() {
        let piece_length = 65_536usize;
        let data: Vec<u8> = (0..piece_length as u32)
            .map(|index| (index % 251) as u8)
            .collect();
        let raw = single_file_torrent("blocks.bin", &data, piece_length);
        let torrent = Torrent::parse(&raw).unwrap();
        assert_eq!(torrent.piece_count(), 1);
        let info_hash = torrent.info_hash;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, request_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            fake_peer_gated(
                listener,
                data,
                piece_length,
                1,
                info_hash,
                requests,
                gate_rx,
            )
        });

        let dir = std::env::temp_dir().join(format!("ftc-blocks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, _progress) = Session::open(
            torrent,
            &dir,
            Options {
                max_peers: 1,
                port: 6881,
                use_dht: false,
                limit_bps: None,
            },
        )
        .unwrap();

        let pause = session.pause_handle();
        let worker = std::thread::spawn(move || session.run(Some(vec![address])));

        request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the peer never saw a block request");
        pause.store(true, Ordering::Relaxed);
        gate_tx.send(()).unwrap();

        assert!(
            request_rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "the client requested another block while paused"
        );

        pause.store(false, Ordering::Relaxed);
        let stats = worker.join().unwrap().unwrap();
        assert_eq!(stats.downloaded, piece_length as u64);

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_lands_between_blocks_and_reports_cancelled() {
        let piece_length = 65_536usize;
        let data: Vec<u8> = (0..piece_length as u32)
            .map(|index| (index % 251) as u8)
            .collect();
        let raw = single_file_torrent("cancel.bin", &data, piece_length);
        let torrent = Torrent::parse(&raw).unwrap();
        let info_hash = torrent.info_hash;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, request_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            fake_peer_gated(
                listener,
                data,
                piece_length,
                1,
                info_hash,
                requests,
                gate_rx,
            )
        });

        let dir = std::env::temp_dir().join(format!("ftc-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, _progress) = Session::open(
            torrent,
            &dir,
            Options {
                max_peers: 1,
                port: 6881,
                use_dht: false,
                limit_bps: None,
            },
        )
        .unwrap();

        let stop = session.stop_handle();
        let worker = std::thread::spawn(move || session.run(Some(vec![address])));

        request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the peer never saw a block request");
        stop.store(true, Ordering::Relaxed);
        gate_tx.send(()).unwrap();

        assert!(
            matches!(worker.join().unwrap(), Err(Error::Cancelled)),
            "stop did not surface as a cancellation"
        );

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rate_limit_slows_the_download_instead_of_stopping_it() {
        let data: Vec<u8> = (0..40_000u32).map(|index| (index % 251) as u8).collect();
        let piece_length = 16_384usize;
        let raw = single_file_torrent("limited.bin", &data, piece_length);
        let torrent = Torrent::parse(&raw).unwrap();
        let pieces = torrent.piece_count();
        let info_hash = torrent.info_hash;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            std::thread::spawn(move || fake_peer(listener, data, piece_length, pieces, info_hash));

        let dir = std::env::temp_dir().join(format!("ftc-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, _progress) = Session::open(
            torrent,
            &dir,
            Options {
                max_peers: 1,
                port: 6881,
                use_dht: false,
                // 40 KB at 80 KB/s cannot finish much inside 300 ms.
                limit_bps: Some(80_000),
            },
        )
        .unwrap();

        let stats = session.run(Some(vec![address])).unwrap();
        assert_eq!(stats.downloaded, 40_000);
        assert!(
            stats.elapsed >= Duration::from_millis(300),
            "40 KB at 80 KB/s finished in {:?}, so the cap was ignored",
            stats.elapsed
        );

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
