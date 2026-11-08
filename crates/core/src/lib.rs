//! A from-scratch BitTorrent engine.
//!
//! Ported from the Node.js implementation: bencode, metainfo and magnet parsing,
//! HTTP/UDP tracker announces, BEP 9/10 metadata exchange, the peer wire protocol,
//! SHA-1 piece verification, multi-file storage, resume scanning, a
//! downloading session, a seeder, and DHT (BEP 5) peer discovery with announce.
//!
//! Not yet ported (tracked in the README): the desktop/web front ends.

pub mod bencode;
pub mod dht;
pub mod error;
pub mod magnet;
pub mod metadata;
pub mod peer;
pub mod seed;
pub mod session;
pub mod storage;
pub mod torrent;
pub mod tracker;

pub use error::{Error, Result};
pub use session::{Options, Progress, Session, Snapshot, Stats};
pub use torrent::Torrent;
