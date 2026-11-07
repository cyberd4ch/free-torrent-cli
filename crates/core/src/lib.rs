//! A from-scratch BitTorrent engine.
//!
//! Ported from the Node.js implementation: bencode, metainfo and magnet parsing,
//! HTTP/UDP tracker announces, the peer wire protocol, SHA-1 piece verification,
//! multi-file storage, resume scanning, and a downloading session.
//!
//! Not yet ported (tracked in the README): DHT (BEP 5), metadata exchange (BEP 9/10),
//! seeding/upload, and the Electron/web front ends.

pub mod bencode;
pub mod error;
pub mod magnet;
pub mod peer;
pub mod session;
pub mod storage;
pub mod torrent;
pub mod tracker;

pub use error::{Error, Result};
pub use session::{Options, Progress, Session, Snapshot, Stats};
pub use torrent::Torrent;
