//! BitTorrent peer wire protocol (BEP 3), framing and message codec.

use crate::error::{Error, Result};
use std::io::{Read, Write};

pub const BLOCK_SIZE: u32 = 16 * 1024;
pub const PROTOCOL: &[u8] = b"BitTorrent protocol";

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        data: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    Extended(u8, Vec<u8>),
    Port(u16),
    Other(u8, Vec<u8>),
}

impl Message {
    pub fn id(&self) -> u8 {
        match self {
            Message::Choke => 0,
            Message::Unchoke => 1,
            Message::Interested => 2,
            Message::NotInterested => 3,
            Message::Have(_) => 4,
            Message::Bitfield(_) => 5,
            Message::Request { .. } => 6,
            Message::Piece { .. } => 7,
            Message::Cancel { .. } => 8,
            Message::Port(_) => 9,
            Message::Extended(_, _) => 20,
            Message::Other(id, _) => *id,
            Message::KeepAlive => 255,
        }
    }
}

pub fn encode(message: &Message) -> Vec<u8> {
    let mut payload = Vec::new();
    match message {
        Message::KeepAlive => return 0u32.to_be_bytes().to_vec(),
        Message::Choke => payload.push(0),
        Message::Unchoke => payload.push(1),
        Message::Interested => payload.push(2),
        Message::NotInterested => payload.push(3),
        Message::Have(index) => {
            payload.push(4);
            payload.extend_from_slice(&index.to_be_bytes());
        }
        Message::Bitfield(bits) => {
            payload.push(5);
            payload.extend_from_slice(bits);
        }
        Message::Request {
            index,
            begin,
            length,
        } => {
            payload.push(6);
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&begin.to_be_bytes());
            payload.extend_from_slice(&length.to_be_bytes());
        }
        Message::Piece { index, begin, data } => {
            payload.push(7);
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&begin.to_be_bytes());
            payload.extend_from_slice(data);
        }
        Message::Cancel {
            index,
            begin,
            length,
        } => {
            payload.push(8);
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&begin.to_be_bytes());
            payload.extend_from_slice(&length.to_be_bytes());
        }
        Message::Port(port) => {
            payload.push(9);
            payload.extend_from_slice(&port.to_be_bytes());
        }
        Message::Extended(extension, data) => {
            payload.push(20);
            payload.push(*extension);
            payload.extend_from_slice(data);
        }
        Message::Other(id, data) => {
            payload.push(*id);
            payload.extend_from_slice(data);
        }
    }
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn decode(payload: &[u8]) -> Result<Message> {
    if payload.is_empty() {
        return Ok(Message::KeepAlive);
    }
    let id = payload[0];
    let body = &payload[1..];
    let need = |count: usize| -> Result<()> {
        if body.len() < count {
            return Err(Error::Peer(format!(
                "message {id} needs {count} bytes, got {}",
                body.len()
            )));
        }
        Ok(())
    };
    Ok(match id {
        0 => Message::Choke,
        1 => Message::Unchoke,
        2 => Message::Interested,
        3 => Message::NotInterested,
        4 => {
            need(4)?;
            Message::Have(u32::from_be_bytes(body[0..4].try_into().unwrap()))
        }
        5 => Message::Bitfield(body.to_vec()),
        6 => {
            need(12)?;
            Message::Request {
                index: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(body[4..8].try_into().unwrap()),
                length: u32::from_be_bytes(body[8..12].try_into().unwrap()),
            }
        }
        7 => {
            need(8)?;
            Message::Piece {
                index: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(body[4..8].try_into().unwrap()),
                data: body[8..].to_vec(),
            }
        }
        8 => {
            need(12)?;
            Message::Cancel {
                index: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(body[4..8].try_into().unwrap()),
                length: u32::from_be_bytes(body[8..12].try_into().unwrap()),
            }
        }
        9 => {
            need(2)?;
            Message::Port(u16::from_be_bytes(body[0..2].try_into().unwrap()))
        }
        20 => {
            need(1)?;
            Message::Extended(body[0], body[1..].to_vec())
        }
        other => Message::Other(other, body.to_vec()),
    })
}

pub fn write_handshake<W: Write>(
    stream: &mut W,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
) -> Result<()> {
    let mut packet = Vec::with_capacity(68);
    packet.push(PROTOCOL.len() as u8);
    packet.extend_from_slice(PROTOCOL);
    packet.extend_from_slice(&[0u8; 8]);
    packet.extend_from_slice(info_hash);
    packet.extend_from_slice(peer_id);
    stream.write_all(&packet)?;
    stream.flush()?;
    Ok(())
}

pub fn read_handshake<R: Read>(stream: &mut R, info_hash: &[u8; 20]) -> Result<[u8; 20]> {
    let mut header = [0u8; 68];
    stream.read_exact(&mut header)?;
    if header[0] as usize != PROTOCOL.len() || &header[1..20] != PROTOCOL {
        return Err(Error::Peer("not a BitTorrent handshake".into()));
    }
    if &header[28..48] != info_hash {
        return Err(Error::Peer(
            "peer answered for a different info hash".into(),
        ));
    }
    let mut peer_id = [0u8; 20];
    peer_id.copy_from_slice(&header[48..68]);
    Ok(peer_id)
}

pub fn read_message<R: Read>(stream: &mut R) -> Result<Message> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > 2 * 1024 * 1024 {
        return Err(Error::Peer(format!(
            "peer message of {length} bytes is too large"
        )));
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    decode(&payload)
}

pub fn write_message<W: Write>(stream: &mut W, message: &Message) -> Result<()> {
    stream.write_all(&encode(message))?;
    stream.flush()?;
    Ok(())
}

pub fn pick_blocks(piece_size: u64, block: u32) -> Vec<(u32, u32)> {
    let mut blocks = Vec::new();
    let mut begin = 0u32;
    while (begin as u64) < piece_size {
        let length = block.min((piece_size - begin as u64) as u32);
        blocks.push((begin, length));
        begin += length;
    }
    blocks
}

/// Test a bitfield for a piece index, tolerating short or absent bitfields.
pub fn has_piece(bitfield: &[u8], index: usize) -> bool {
    let byte = index / 8;
    bitfield
        .get(byte)
        .map(|value| value & (0x80 >> (index % 8)) != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_every_message() {
        let messages = vec![
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(7),
            Message::Bitfield(vec![0b1010_0000]),
            Message::Request {
                index: 1,
                begin: 16384,
                length: 16384,
            },
            Message::Piece {
                index: 2,
                begin: 0,
                data: vec![1, 2, 3],
            },
            Message::Cancel {
                index: 3,
                begin: 0,
                length: 16384,
            },
            Message::Port(6881),
            Message::Extended(1, vec![9, 9]),
        ];
        let mut buffer = Vec::new();
        for message in &messages {
            buffer.extend(encode(message));
        }
        let mut cursor = Cursor::new(buffer);
        for message in &messages {
            assert_eq!(&read_message(&mut cursor).unwrap(), message);
        }
    }

    #[test]
    fn handshake_round_trip_checks_hash() {
        let info_hash = [3u8; 20];
        let peer_id = *b"-FT0300-abcdefghijkl";
        let mut buffer = Vec::new();
        write_handshake(&mut buffer, &info_hash, &peer_id).unwrap();
        let mut cursor = Cursor::new(buffer.clone());
        assert_eq!(read_handshake(&mut cursor, &info_hash).unwrap(), peer_id);

        let mut cursor = Cursor::new(buffer);
        assert!(read_handshake(&mut cursor, &[4u8; 20]).is_err());
    }

    #[test]
    fn rejects_oversized_frames() {
        // Header claims a frame far beyond the 2 MiB cap; the body is never buffered.
        let buffer = 3_000_000u32.to_be_bytes().to_vec();
        assert!(read_message(&mut Cursor::new(buffer)).is_err());
    }

    #[test]
    fn splits_pieces_into_blocks() {
        assert_eq!(pick_blocks(16384, BLOCK_SIZE), vec![(0, 16384)]);
        assert_eq!(
            pick_blocks(40000, BLOCK_SIZE),
            vec![(0, 16384), (16384, 16384), (32768, 7232)]
        );
    }

    #[test]
    fn reads_bitfield_bits_in_msb_order() {
        let bits = [0b1000_0001, 0b0100_0000];
        assert!(has_piece(&bits, 0));
        assert!(!has_piece(&bits, 1));
        assert!(has_piece(&bits, 7));
        assert!(has_piece(&bits, 9));
        assert!(!has_piece(&bits, 200));
    }
}
