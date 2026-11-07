//! Metainfo (`.torrent`) parsing.

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use sha1::{Digest, Sha1};
use std::path::{Component, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct FileSpec {
    pub path: PathBuf,
    pub length: u64,
    /// Byte offset of this file inside the torrent's single logical stream.
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub struct Torrent {
    pub name: String,
    pub announce: Option<String>,
    pub trackers: Vec<String>,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    pub files: Vec<FileSpec>,
    pub total_length: u64,
    pub info_hash: [u8; 20],
    pub raw_info: Vec<u8>,
    pub private: bool,
}

impl Torrent {
    pub fn parse(input: &[u8]) -> Result<Self> {
        let (value, info_span) = bencode::decode_with_info(input);
        let root = value?;
        let info_span =
            info_span.ok_or_else(|| Error::Torrent("missing info dictionary".into()))?;
        let raw_info = input[info_span].to_vec();
        let info = root
            .get("info")
            .ok_or_else(|| Error::Torrent("missing info dictionary".into()))?;

        let name = info
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Torrent("missing info.name".into()))?;
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
        {
            return Err(Error::Torrent(format!("unsafe torrent name {name:?}")));
        }

        let piece_length = info
            .get("piece length")
            .and_then(Value::as_int)
            .filter(|length| *length > 0)
            .ok_or_else(|| Error::Torrent("missing info.piece length".into()))?
            as u64;

        let pieces_raw = info
            .get("pieces")
            .and_then(Value::as_bytes)
            .ok_or_else(|| Error::Torrent("missing info.pieces".into()))?;
        if pieces_raw.len() % 20 != 0 {
            return Err(Error::Torrent(
                "info.pieces is not a multiple of 20 bytes".into(),
            ));
        }
        let pieces: Vec<[u8; 20]> = pieces_raw.as_chunks::<20>().0.to_vec();

        let mut files = Vec::new();
        let mut offset = 0u64;
        match info.get("files").and_then(Value::as_list) {
            Some(entries) => {
                if entries.is_empty() {
                    return Err(Error::Torrent("info.files is empty".into()));
                }
                for entry in entries {
                    let length = entry
                        .get("length")
                        .and_then(Value::as_int)
                        .filter(|length| *length >= 0)
                        .ok_or_else(|| Error::Torrent("file entry missing length".into()))?
                        as u64;
                    let components = entry
                        .get("path")
                        .and_then(Value::as_list)
                        .ok_or_else(|| Error::Torrent("file entry missing path".into()))?;
                    let mut path = PathBuf::new();
                    for component in components {
                        let part = component
                            .as_str()
                            .ok_or_else(|| Error::Torrent("path component is not UTF-8".into()))?;
                        let parsed = PathBuf::from(part);
                        if part.is_empty()
                            || part.contains('/')
                            || part.contains('\\')
                            || parsed
                                .components()
                                .any(|piece| !matches!(piece, Component::Normal(_)))
                        {
                            return Err(Error::Torrent(format!("unsafe path component {part:?}")));
                        }
                        path.push(part);
                    }
                    if path.as_os_str().is_empty() {
                        return Err(Error::Torrent("file entry has an empty path".into()));
                    }
                    files.push(FileSpec {
                        path,
                        length,
                        offset,
                    });
                    offset += length;
                }
            }
            None => {
                let length = info
                    .get("length")
                    .and_then(Value::as_int)
                    .filter(|length| *length >= 0)
                    .ok_or_else(|| Error::Torrent("missing info.length".into()))?
                    as u64;
                files.push(FileSpec {
                    path: PathBuf::from(name),
                    length,
                    offset: 0,
                });
                offset = length;
            }
        }

        let expected_pieces = offset.div_ceil(piece_length) as usize;
        if pieces.len() != expected_pieces {
            return Err(Error::Torrent(format!(
                "piece count {} does not match {} bytes in {} pieces",
                pieces.len(),
                offset,
                expected_pieces
            )));
        }

        let mut trackers = Vec::new();
        if let Some(announce) = root.get("announce").and_then(Value::as_str) {
            trackers.push(announce.to_string());
        }
        if let Some(tiers) = root.get("announce-list").and_then(Value::as_list) {
            for tier in tiers {
                for tracker in tier.as_list().unwrap_or_default() {
                    if let Some(url) = tracker.as_str() {
                        trackers.push(url.to_string());
                    }
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        trackers.retain(|url| {
            (url.starts_with("http://") || url.starts_with("https://") || url.starts_with("udp://"))
                && seen.insert(url.clone())
        });

        let private = info
            .get("private")
            .and_then(Value::as_int)
            .map(|value| value == 1)
            .unwrap_or(false);

        let mut hasher = Sha1::new();
        hasher.update(&raw_info);
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&hasher.finalize());

        Ok(Torrent {
            name: name.to_string(),
            announce: root
                .get("announce")
                .and_then(Value::as_str)
                .map(str::to_string),
            trackers,
            piece_length,
            pieces,
            files,
            total_length: offset,
            info_hash,
            raw_info,
            private,
        })
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn piece_size(&self, index: usize) -> u64 {
        let start = index as u64 * self.piece_length;
        (self.total_length - start).min(self.piece_length)
    }

    pub fn last_piece(&self) -> Option<usize> {
        self.pieces.len().checked_sub(1)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::bencode::{encode, Value};

    /// Build a valid single-file torrent whose piece hashes match `data`.
    pub fn single_file_torrent(name: &str, data: &[u8], piece_length: usize) -> Vec<u8> {
        let pieces: Vec<u8> = data
            .chunks(piece_length)
            .flat_map(|chunk| {
                let mut hasher = Sha1::new();
                hasher.update(chunk);
                hasher.finalize().to_vec()
            })
            .collect();
        let info = Value::Dict(vec![
            (b"length".to_vec(), Value::Int(data.len() as i64)),
            (b"name".to_vec(), Value::Bytes(name.as_bytes().to_vec())),
            (b"piece length".to_vec(), Value::Int(piece_length as i64)),
            (b"pieces".to_vec(), Value::Bytes(pieces)),
        ]);
        encode(&Value::Dict(vec![
            (
                b"announce".to_vec(),
                Value::Bytes(b"http://tracker.test/announce".to_vec()),
            ),
            (b"info".to_vec(), info),
        ]))
    }

    #[test]
    fn parses_single_file_torrent() {
        let raw = single_file_torrent("hello.bin", b"hello world", 4);
        let torrent = Torrent::parse(&raw).unwrap();
        assert_eq!(torrent.name, "hello.bin");
        assert_eq!(torrent.total_length, 11);
        assert_eq!(torrent.piece_count(), 3);
        assert_eq!(torrent.piece_size(0), 4);
        assert_eq!(torrent.piece_size(2), 3);
        assert_eq!(torrent.files.len(), 1);
        assert_eq!(torrent.info_hash.len(), 20);
    }

    #[test]
    fn parses_multi_file_layout_and_offsets() {
        let data = b"abcdefgh";
        let pieces: Vec<u8> = data
            .chunks(4)
            .flat_map(|chunk| {
                let mut hasher = Sha1::new();
                hasher.update(chunk);
                hasher.finalize().to_vec()
            })
            .collect();
        let files = Value::List(vec![
            Value::Dict(vec![
                (b"length".to_vec(), Value::Int(3)),
                (
                    b"path".to_vec(),
                    Value::List(vec![Value::Bytes(b"a.txt".to_vec())]),
                ),
            ]),
            Value::Dict(vec![
                (b"length".to_vec(), Value::Int(5)),
                (
                    b"path".to_vec(),
                    Value::List(vec![
                        Value::Bytes(b"sub".to_vec()),
                        Value::Bytes(b"b.txt".to_vec()),
                    ]),
                ),
            ]),
        ]);
        let info = Value::Dict(vec![
            (b"files".to_vec(), files),
            (b"name".to_vec(), Value::Bytes(b"bundle".to_vec())),
            (b"piece length".to_vec(), Value::Int(4)),
            (b"pieces".to_vec(), Value::Bytes(pieces)),
        ]);
        let raw = encode(&Value::Dict(vec![
            (
                b"announce".to_vec(),
                Value::Bytes(b"udp://tracker.test:1337".to_vec()),
            ),
            (b"info".to_vec(), info),
        ]));
        let torrent = Torrent::parse(&raw).unwrap();
        assert_eq!(torrent.files[0].offset, 0);
        assert_eq!(torrent.files[1].offset, 3);
        assert_eq!(torrent.files[1].path, PathBuf::from("sub/b.txt"));
        assert_eq!(torrent.total_length, 8);
        assert_eq!(
            torrent.trackers,
            vec!["udp://tracker.test:1337".to_string()]
        );
    }

    #[test]
    fn rejects_path_traversal() {
        let info = Value::Dict(vec![
            (
                b"files".to_vec(),
                Value::List(vec![Value::Dict(vec![
                    (b"length".to_vec(), Value::Int(1)),
                    (
                        b"path".to_vec(),
                        Value::List(vec![
                            Value::Bytes(b"..".to_vec()),
                            Value::Bytes(b"x".to_vec()),
                        ]),
                    ),
                ])]),
            ),
            (b"name".to_vec(), Value::Bytes(b"bad".to_vec())),
            (b"piece length".to_vec(), Value::Int(4)),
            (b"pieces".to_vec(), Value::Bytes(vec![0u8; 20])),
        ]);
        let raw = encode(&Value::Dict(vec![(b"info".to_vec(), info)]));
        assert!(Torrent::parse(&raw).is_err());
    }

    #[test]
    fn rejects_piece_count_mismatch() {
        let raw = single_file_torrent("x", b"hello world", 4);
        // Drop a piece hash: the encoded size must no longer agree with the file size.
        let value = bencode::decode(&raw).unwrap();
        let Value::Dict(mut root) = value else {
            unreachable!()
        };
        let info_index = root.iter().position(|(key, _)| key == b"info").unwrap();
        let Value::Dict(mut info) = root[info_index].1.clone() else {
            unreachable!()
        };
        let pieces_index = info.iter().position(|(key, _)| key == b"pieces").unwrap();
        info[pieces_index].1 = Value::Bytes(vec![0u8; 20]);
        root[info_index].1 = Value::Dict(info);
        assert!(Torrent::parse(&encode(&Value::Dict(root))).is_err());
    }
}
