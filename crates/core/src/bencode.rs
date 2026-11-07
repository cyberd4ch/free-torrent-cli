//! Bencode encoder/decoder.
//!
//! The decoder records the byte span of the top-level `info` dictionary, because the
//! BitTorrent info-hash is SHA-1 over those exact bytes — re-encoding a parsed value
//! is not guaranteed to reproduce them byte for byte.

use crate::error::{Error, Result};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(Vec<(Vec<u8>, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(entries) => entries
                .iter()
                .find(|(name, _)| name.as_slice() == key.as_bytes())
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&[(Vec<u8>, Value)]> {
        match self {
            Value::Dict(entries) => Some(entries),
            _ => None,
        }
    }
}

pub fn decode(input: &[u8]) -> Result<Value> {
    decode_with_info(input).0
}

/// Decode and also return the span of the top-level `info` value, if present.
pub fn decode_with_info(input: &[u8]) -> (Result<Value>, Option<Range<usize>>) {
    let mut parser = Parser { input, pos: 0 };
    let mut info = None;
    let value = parser.parse_value(0, &mut info);
    let span = info;
    (value, span)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse_value(&mut self, depth: usize, info: &mut Option<Range<usize>>) -> Result<Value> {
        if depth > 64 {
            return Err(Error::Bencode("nesting too deep".into()));
        }
        match self.peek()? {
            b'i' => self.parse_int(),
            b'l' => self.parse_list(depth, info),
            b'd' => self.parse_dict(depth, info),
            b'0'..=b'9' => self.parse_bytes(),
            other => Err(Error::Bencode(format!(
                "unexpected byte {:?}",
                other as char
            ))),
        }
    }

    fn peek(&self) -> Result<u8> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or_else(|| Error::Bencode("unexpected end of input".into()))
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek()? != byte {
            return Err(Error::Bencode(format!("expected {:?}", byte as char)));
        }
        self.pos += 1;
        Ok(())
    }

    fn parse_int(&mut self) -> Result<Value> {
        self.expect(b'i')?;
        let start = self.pos;
        while self.peek()? != b'e' {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| Error::Bencode("invalid integer".into()))?;
        self.pos += 1;
        text.parse::<i64>()
            .map(Value::Int)
            .map_err(|_| Error::Bencode(format!("invalid integer {text:?}")))
    }

    fn parse_bytes(&mut self) -> Result<Value> {
        let start = self.pos;
        while self.peek()?.is_ascii_digit() {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| Error::Bencode("invalid length".into()))?;
        let len: usize = text
            .parse()
            .map_err(|_| Error::Bencode(format!("invalid length {text:?}")))?;
        self.expect(b':')?;
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| Error::Bencode("length overflow".into()))?;
        if end > self.input.len() {
            return Err(Error::Bencode("string runs past end of input".into()));
        }
        let out = self.input[self.pos..end].to_vec();
        self.pos = end;
        Ok(Value::Bytes(out))
    }

    fn parse_list(&mut self, depth: usize, info: &mut Option<Range<usize>>) -> Result<Value> {
        self.expect(b'l')?;
        let mut items = Vec::new();
        while self.peek()? != b'e' {
            items.push(self.parse_value(depth + 1, info)?);
        }
        self.pos += 1;
        Ok(Value::List(items))
    }

    fn parse_dict(&mut self, depth: usize, info: &mut Option<Range<usize>>) -> Result<Value> {
        self.expect(b'd')?;
        let mut entries = Vec::new();
        while self.peek()? != b'e' {
            let key = match self.parse_bytes()? {
                Value::Bytes(key) => key,
                _ => unreachable!(),
            };
            let start = self.pos;
            let value = self.parse_value(depth + 1, info)?;
            if depth == 0 && key.as_slice() == b"info" {
                *info = Some(start..self.pos);
            }
            entries.push((key, value));
        }
        self.pos += 1;
        Ok(Value::Dict(entries))
    }
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(number) => {
            out.push(b'i');
            out.extend_from_slice(number.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(entries) => {
            let mut sorted: Vec<&(Vec<u8>, Value)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(b'd');
            for (key, item) in sorted {
                out.extend_from_slice(key.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(key);
                encode_into(item, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_nested_values() {
        let value = Value::Dict(vec![
            (b"name".to_vec(), Value::Bytes(b"file.iso".to_vec())),
            (b"len".to_vec(), Value::Int(42)),
            (
                b"list".to_vec(),
                Value::List(vec![Value::Int(1), Value::Bytes(b"x".to_vec())]),
            ),
        ]);
        let encoded = encode(&value);
        // Encoding sorts dictionary keys, so compare canonically rather than by insertion order.
        assert_eq!(encode(&decode(&encoded).unwrap()), encoded);
    }

    #[test]
    fn sorts_dictionary_keys() {
        let encoded = encode(&Value::Dict(vec![
            (b"b".to_vec(), Value::Int(1)),
            (b"a".to_vec(), Value::Int(2)),
        ]));
        assert_eq!(encoded, b"d1:ai2e1:bi1ee");
    }

    #[test]
    fn records_info_span() {
        let raw = b"d4:infod4:name4:testee";
        let (value, span) = decode_with_info(raw);
        let value = value.unwrap();
        assert!(value.get("info").is_some());
        let span = span.unwrap();
        assert_eq!(&raw[span], b"d4:name4:teste");
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(decode(b"d4:info").is_err());
        assert!(decode(b"i12").is_err());
        assert!(decode(b"5:abc").is_err());
    }

    #[test]
    fn rejects_absurd_nesting() {
        let deep = format!("{}e", "l".repeat(200));
        assert!(decode(deep.as_bytes()).is_err());
    }
}
