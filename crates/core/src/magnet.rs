//! Magnet link parsing (BEP 9 metadata-free downloads).

use crate::error::{Error, Result};
use percent_encoding::percent_decode_str;

#[derive(Debug, Clone, PartialEq)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    pub name: Option<String>,
    pub trackers: Vec<String>,
}

pub fn parse(uri: &str) -> Result<Magnet> {
    let rest = uri
        .strip_prefix("magnet:?")
        .ok_or_else(|| Error::Magnet("not a magnet URI".into()))?;

    let mut info_hash = None;
    let mut name = None;
    let mut trackers = Vec::new();

    for pair in rest.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| Error::Magnet("query value is not UTF-8".into()))?
            .to_string();
        match key {
            "xt" => {
                for urn in value.split('&') {
                    if let Some(hash) = urn.strip_prefix("urn:btih:") {
                        info_hash = Some(decode_info_hash(hash)?);
                    }
                }
            }
            "dn" => name = Some(value),
            "tr" if is_supported(value.as_str()) => trackers.push(value),
            _ => {}
        }
    }

    let info_hash = info_hash.ok_or_else(|| Error::Magnet("missing xt=urn:btih".into()))?;
    trackers.sort();
    trackers.dedup();

    Ok(Magnet {
        info_hash,
        name,
        trackers,
    })
}

fn is_supported(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("udp://")
}

fn decode_info_hash(text: &str) -> Result<[u8; 20]> {
    let mut hash = [0u8; 20];
    if text.len() == 40 {
        let bytes = hex::decode(text).map_err(|_| Error::Magnet("invalid hex info hash".into()))?;
        hash.copy_from_slice(&bytes);
        return Ok(hash);
    }
    if text.len() == 32 {
        let bytes = base32::decode(base32::Alphabet::RFC4648 { padding: false }, text)
            .ok_or_else(|| Error::Magnet("invalid base32 info hash".into()))?;
        if bytes.len() != 20 {
            return Err(Error::Magnet("base32 info hash is not 20 bytes".into()));
        }
        hash.copy_from_slice(&bytes);
        return Ok(hash);
    }
    Err(Error::Magnet(
        "info hash is neither 40 hex nor 32 base32 characters".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parses_hex_hash_name_and_trackers() {
        let uri = format!(
            "magnet:?xt=urn:btih:{HEX}&dn=Some%20File&tr=udp%3A%2F%2Ftracker.one%3A1337&tr=https%3A%2F%2Ftracker.two%2Fa"
        );
        let magnet = parse(&uri).unwrap();
        assert_eq!(hex::encode(magnet.info_hash), HEX);
        assert_eq!(magnet.name.as_deref(), Some("Some File"));
        assert_eq!(magnet.trackers.len(), 2);
    }

    #[test]
    fn parses_base32_hash() {
        let bytes: Vec<u8> = (0..20u8).collect();
        let encoded = base32::encode(base32::Alphabet::RFC4648 { padding: false }, &bytes);
        let magnet = parse(&format!("magnet:?xt=urn:btih:{encoded}")).unwrap();
        assert_eq!(magnet.info_hash.to_vec(), bytes);
    }

    #[test]
    fn rejects_missing_hash_and_bad_input() {
        assert!(parse("magnet:?dn=nameless").is_err());
        assert!(parse("http://example.com").is_err());
        assert!(parse("magnet:?xt=urn:btih:abc").is_err());
    }

    #[test]
    fn drops_unsupported_trackers() {
        let uri = format!("magnet:?xt=urn:btih:{HEX}&tr=wss%3A%2F%2Ftracker.test");
        assert!(parse(&uri).unwrap().trackers.is_empty());
    }
}
