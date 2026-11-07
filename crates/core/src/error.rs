use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Bencode(String),
    Torrent(String),
    Magnet(String),
    Tracker(String),
    Peer(String),
    Storage(String),
    Dht(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "io error: {err}"),
            Error::Bencode(msg) => write!(f, "bencode error: {msg}"),
            Error::Torrent(msg) => write!(f, "torrent error: {msg}"),
            Error::Magnet(msg) => write!(f, "magnet error: {msg}"),
            Error::Tracker(msg) => write!(f, "tracker error: {msg}"),
            Error::Peer(msg) => write!(f, "peer error: {msg}"),
            Error::Storage(msg) => write!(f, "storage error: {msg}"),
            Error::Dht(msg) => write!(f, "dht error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}
