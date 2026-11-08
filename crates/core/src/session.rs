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

#[derive(Debug, Clone)]
pub struct Options {
    pub max_peers: usize,
    pub port: u16,
    /// Discover peers from the DHT (BEP 5) in addition to the trackers.
    pub use_dht: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_peers: 6,
            port: 6881,
            use_dht: true,
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
}

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub total: u64,
    pub downloaded: u64,
    pub pieces: usize,
    pub verified: usize,
    pub active_peers: usize,
    pub done: bool,
    pub elapsed: Duration,
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
        });
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

    /// Run to completion, using `explicit_peers` when supplied (tests, `--peer`).
    pub fn run(mut self, explicit_peers: Option<Vec<SocketAddr>>) -> Result<Stats> {
        let started = Instant::now();
        let mut attempts = 0;
        // Unless the caller handed over peers by hand, join the DHT so that
        // discovery does not rely on trackers alone.
        if explicit_peers.is_none() && self.options.use_dht {
            match Dht::boot(dht::random_node_id(), self.options.port) {
                Ok(node) => self.dht = Some(node),
                Err(err) => eprintln!("DHT unavailable, trackers only: {err}"),
            }
        }
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
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

            self.fill_peers(explicit_peers.as_deref(), attempts)?;
            if self.peers.lock().unwrap().is_empty() {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }

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
}

impl Shared {
    fn worker_loop(&self) {
        loop {
            let piece = match self.missing.lock().unwrap().pop_front() {
                Some(piece) => piece,
                None => return,
            };
            let address = match self.peers.lock().unwrap().pop_front() {
                Some(address) => address,
                None => {
                    self.missing.lock().unwrap().push_front(piece);
                    return;
                }
            };

            self.progress.active_peers.fetch_add(1, Ordering::Relaxed);
            let result = self.fetch_piece(address, piece);
            self.progress.active_peers.fetch_sub(1, Ordering::Relaxed);

            match result {
                Ok(data) => match self.commit(piece, &data) {
                    Ok(()) => {}
                    Err(err) => {
                        eprintln!("piece {piece} rejected: {err}");
                        self.missing.lock().unwrap().push_back(piece);
                    }
                },
                Err(err) => {
                    eprintln!("peer {address} failed on piece {piece}: {err}");
                    self.missing.lock().unwrap().push_front(piece);
                }
            }
        }
    }

    fn commit(&self, index: usize, data: &[u8]) -> Result<()> {
        let mut hasher = Sha1::new();
        hasher.update(data);
        let digest = hasher.finalize();
        if digest.as_slice() != self.torrent.pieces[index] {
            return Err(Error::Peer(format!(
                "piece {index} failed SHA-1 verification"
            )));
        }
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

    fn fetch_piece(&self, address: SocketAddr, index: usize) -> Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        peer::write_handshake(&mut stream, &self.torrent.info_hash, &self.peer_id)?;
        peer::read_handshake(&mut stream, &self.torrent.info_hash)?;
        peer::write_message(&mut stream, &Message::Interested)?;

        let mut bitfield: Option<Vec<u8>> = None;
        loop {
            match peer::read_message(&mut stream)? {
                Message::Bitfield(bits) => bitfield = Some(bits),
                Message::Unchoke => break,
                Message::Choke => {}
                Message::KeepAlive | Message::Have(_) => {}
                other => {
                    return Err(Error::Peer(format!(
                        "expected unchoke, got message id {}",
                        other.id()
                    )))
                }
            }
        }
        if let Some(bits) = &bitfield {
            if !peer::has_piece(bits, index) {
                return Err(Error::Peer(format!("peer does not have piece {index}")));
            }
        }

        let mut piece = vec![0u8; self.torrent.piece_size(index) as usize];
        for (begin, length) in peer::pick_blocks(piece.len() as u64, BLOCK_SIZE) {
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
                    Message::KeepAlive | Message::Have(_) => {}
                    other => {
                        return Err(Error::Peer(format!(
                            "expected block, got message id {}",
                            other.id()
                        )))
                    }
                }
            }
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

    /// A minimal seeder: handshake, full bitfield, unchoke, then answer every request.
    fn fake_peer(
        listener: TcpListener,
        data: Vec<u8>,
        piece_length: usize,
        pieces: usize,
        info_hash: [u8; 20],
    ) {
        for _ in 0..pieces {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            if peer::read_handshake(&mut stream, &info_hash).is_err() {
                continue;
            }
            let _ = peer::write_handshake(&mut stream, &info_hash, &[7u8; 20]);
            let mut bits = vec![0u8; pieces.div_ceil(8)];
            for index in 0..pieces {
                bits[index / 8] |= 0x80 >> (index % 8);
            }
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
}
