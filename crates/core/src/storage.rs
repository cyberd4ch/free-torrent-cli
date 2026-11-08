//! On-disk storage: maps the torrent's logical byte stream onto files and verifies pieces.

use crate::error::{Error, Result};
use crate::torrent::Torrent;
use sha1::{Digest, Sha1};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    offset: u64,
    length: u64,
}

#[derive(Debug)]
pub struct Storage {
    root: PathBuf,
    entries: Vec<Entry>,
    total: u64,
}

impl Storage {
    pub fn open(root: &Path, torrent: &Torrent) -> Result<Self> {
        let root = root.to_path_buf();
        let mut entries = Vec::with_capacity(torrent.files.len());
        for file in &torrent.files {
            let path = root.join(&file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Allocate the file so resume can distinguish "present but zero" from "missing".
            // Keep existing bytes: resume relies on what is already on disk.
            let handle = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            if handle.metadata()?.len() != file.length {
                handle.set_len(file.length)?;
            }
            entries.push(Entry {
                path,
                offset: file.offset,
                length: file.length,
            });
        }
        Ok(Storage {
            root,
            entries,
            total: torrent.total_length,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write_piece(&self, index: usize, piece_length: u64, data: &[u8]) -> Result<()> {
        let start = index as u64 * piece_length;
        self.write_at(start, data)
    }

    pub fn write_at(&self, start: u64, data: &[u8]) -> Result<()> {
        if start + data.len() as u64 > self.total {
            return Err(Error::Storage("write runs past torrent end".into()));
        }
        let end = start + data.len() as u64;
        let mut written = 0usize;
        for entry in &self.entries {
            let entry_end = entry.offset + entry.length;
            if entry_end <= start || entry.offset >= end {
                continue;
            }
            let local_start = start.max(entry.offset) - entry.offset;
            let local_end = end.min(entry_end) - entry.offset;
            let slice_start = (start.max(entry.offset) - start) as usize;
            let slice_end = slice_start + (local_end - local_start) as usize;
            let mut handle = OpenOptions::new().write(true).open(&entry.path)?;
            handle.seek(SeekFrom::Start(local_start))?;
            handle.write_all(&data[slice_start..slice_end])?;
            written += slice_end - slice_start;
        }
        if written != data.len() {
            return Err(Error::Storage(format!(
                "mapped {written} of {} bytes at offset {start}",
                data.len()
            )));
        }
        Ok(())
    }

    pub fn read_at(&self, start: u64, buf: &mut [u8]) -> Result<()> {
        if start + buf.len() as u64 > self.total {
            return Err(Error::Storage("read runs past torrent end".into()));
        }
        let end = start + buf.len() as u64;
        let mut filled = 0usize;
        for entry in &self.entries {
            let entry_end = entry.offset + entry.length;
            if entry_end <= start || entry.offset >= end {
                continue;
            }
            let local_start = start.max(entry.offset) - entry.offset;
            let local_end = end.min(entry_end) - entry.offset;
            let count = (local_end - local_start) as usize;
            let mut handle = File::open(&entry.path)?;
            handle.seek(SeekFrom::Start(local_start))?;
            handle.read_exact(&mut buf[filled..filled + count])?;
            filled += count;
        }
        if filled != buf.len() {
            return Err(Error::Storage(format!(
                "mapped {filled} of {} bytes at offset {start}",
                buf.len()
            )));
        }
        Ok(())
    }

    /// SHA-1 verify every piece already on disk. Returns which pieces are complete.
    pub fn verify_existing(&self, torrent: &Torrent) -> Result<Vec<bool>> {
        let mut have = Vec::with_capacity(torrent.piece_count());
        for index in 0..torrent.piece_count() {
            let size = torrent.piece_size(index);
            let mut buf = vec![0u8; size as usize];
            let start = index as u64 * torrent.piece_length;
            let exists = self
                .entries
                .iter()
                .any(|entry| entry.offset < start + size && entry.offset + entry.length > start);
            if !exists {
                have.push(false);
                continue;
            }
            match self.read_at(start, &mut buf) {
                Ok(()) => {
                    let mut hasher = Sha1::new();
                    hasher.update(&buf);
                    let digest = hasher.finalize();
                    have.push(digest.as_slice() == torrent.pieces[index]);
                }
                Err(_) => have.push(false),
            }
        }
        Ok(have)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::tests::single_file_torrent;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ftc-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_reads_single_file_torrent() {
        let data = b"hello torrent world";
        let raw = single_file_torrent("data.bin", data, 8);
        let torrent = Torrent::parse(&raw).unwrap();
        let root = temp_root("single");
        let storage = Storage::open(&root, &torrent).unwrap();

        storage
            .write_piece(0, torrent.piece_length, &data[0..8])
            .unwrap();
        storage
            .write_piece(1, torrent.piece_length, &data[8..16])
            .unwrap();
        storage
            .write_piece(2, torrent.piece_length, &data[16..])
            .unwrap();

        let mut round = vec![0u8; data.len()];
        storage.read_at(0, &mut round).unwrap();
        assert_eq!(round, data);
        assert!(storage
            .verify_existing(&torrent)
            .unwrap()
            .iter()
            .all(|ok| *ok));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detects_corrupt_pieces_on_resume() {
        let data = b"abcdefghijklmnop";
        let raw = single_file_torrent("data.bin", data, 8);
        let torrent = Torrent::parse(&raw).unwrap();
        let root = temp_root("corrupt");
        let storage = Storage::open(&root, &torrent).unwrap();
        storage
            .write_piece(0, torrent.piece_length, &data[0..8])
            .unwrap();
        storage
            .write_piece(1, torrent.piece_length, b"XXXXXXXX")
            .unwrap();

        let have = storage.verify_existing(&torrent).unwrap();
        assert_eq!(have, vec![true, false]);

        let _ = std::fs::remove_dir_all(&root);
    }
}
