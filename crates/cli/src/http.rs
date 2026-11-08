//! A blocking HTTP/1.1 server small enough to keep the binary dependency-free.
//!
//! The UI is local-only, so this deliberately supports one request per
//! connection, no keep-alive, no chunked bodies, no TLS: parse a request head,
//! read `Content-Length` bytes, answer, close.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
}

impl Request {
    pub fn read(stream: &TcpStream) -> std::io::Result<Request> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            head.push_str(&line);
            if head.len() > MAX_HEAD {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request head too large",
                ));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }

        let (method, path) = parse_request_line(&head)?;
        let length = content_length(&head).min(MAX_BODY);
        let mut body = vec![0u8; length];
        if length > 0 {
            reader.read_exact(&mut body)?;
        }
        Ok(Request {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

fn parse_request_line(head: &str) -> std::io::Result<(String, String)> {
    let line = head.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }
    // Drop any query string; every route here lives at a fixed path.
    let path = target.split('?').next().unwrap_or_default();
    Ok((method.to_string(), path.to_string()))
}

fn content_length(head: &str) -> usize {
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

pub fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// `a=1&b=two+words` → `[("a", "1"), ("b", "two words")]`.
pub fn form_pairs(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape a string for a hand-built JSON document.
pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_method_and_path_without_query() {
        let (method, path) = parse_request_line("POST /api/add?x=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/add");
    }

    #[test]
    fn rejects_a_garbage_request_line() {
        assert!(parse_request_line("\r\n").is_err());
    }

    #[test]
    fn reads_content_length_case_insensitively() {
        let head = "POST / HTTP/1.1\r\nContent-Length: 12\r\ncontent-type: text/plain\r\n\r\n";
        assert_eq!(content_length(head), 12);
        assert_eq!(content_length("GET / HTTP/1.1\r\n\r\n"), 0);
    }

    #[test]
    fn decodes_form_bodies() {
        let pairs = form_pairs("target=magnet%3A%3Fxt%3Durn%3Abtih%3Aabc&output=two+words");
        assert_eq!(pairs[0].0, "target");
        assert_eq!(pairs[0].1, "magnet:?xt=urn:btih:abc");
        assert_eq!(pairs[1].1, "two words");
    }

    #[test]
    fn escapes_json_strings() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("line\n"), "line\\n");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }
}
