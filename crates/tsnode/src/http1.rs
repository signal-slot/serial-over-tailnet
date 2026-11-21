//! Tiny HTTP/1.1 helpers: enough to issue `GET /key`, to perform the ts2021 and
//! DERP `Upgrade:` requests and to parse the response head.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
#[cfg(feature = "std")]
use std::io::{self, Read, Write};

#[derive(Debug, Default)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Bytes read past the end of the head (belong to the upgraded stream/body).
    pub leftover: Vec<u8>,
}

impl ResponseHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Why a response head could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    TooLarge,
    NotUtf8,
    BadStatusLine,
}

impl core::fmt::Display for HeadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            HeadError::TooLarge => "http response head too large",
            HeadError::NotUtf8 => "non-utf8 http response head",
            HeadError::BadStatusLine => "bad http status line",
        })
    }
}

const MAX_HEAD: usize = 16 * 1024;

/// Parses an HTTP/1.x response head from the start of `buf`: `Ok(None)`
/// while the head is still incomplete. The bytes after it are `leftover`.
pub fn parse_response_head(buf: &[u8]) -> Result<Option<ResponseHead>, HeadError> {
    let Some(end) = find_head_end(buf) else {
        return if buf.len() > MAX_HEAD {
            Err(HeadError::TooLarge)
        } else {
            Ok(None)
        };
    };
    let head = core::str::from_utf8(&buf[..end]).map_err(|_| HeadError::NotUtf8)?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(HeadError::BadStatusLine)?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Some(ResponseHead {
        status,
        headers,
        leftover: buf[end + 4..].to_vec(),
    }))
}

/// Reads an HTTP/1.x response head, byte-wise buffered, from `r`.
#[cfg(feature = "std")]
pub fn read_response_head(r: &mut dyn Read) -> io::Result<ResponseHead> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 256];
    loop {
        let n = r.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "eof in http response head",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        match parse_response_head(&buf) {
            Ok(Some(head)) => return Ok(head),
            Ok(None) => {}
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        }
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Reads a body given a parsed head: honours `Content-Length`, chunked
/// encoding, or reads to EOF.
#[cfg(feature = "std")]
pub fn read_body(r: &mut dyn Read, head: &ResponseHead, max: usize) -> io::Result<Vec<u8>> {
    let mut body = head.leftover.clone();
    if let Some(len) = head
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        while body.len() < len {
            let mut tmp = [0u8; 1024];
            let n = r.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
        return Ok(body);
    }
    if head
        .header("transfer-encoding")
        .map(|v| v.eq_ignore_ascii_case("chunked"))
        .unwrap_or(false)
    {
        // Read everything then de-chunk (bodies here are tiny).
        let mut raw = body;
        let mut tmp = [0u8; 1024];
        loop {
            let n = r.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&tmp[..n]);
            if raw.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
            }
            if raw.ends_with(b"0\r\n\r\n") {
                break;
            }
        }
        return dechunk(&raw);
    }
    let mut tmp = [0u8; 1024];
    loop {
        let n = r.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
    }
    Ok(body)
}

#[cfg(feature = "std")]
fn dechunk(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = raw[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad chunk"))?;
        let size_str = std::str::from_utf8(&raw[pos..pos + line_end]).unwrap_or("");
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        pos += line_end + 2;
        if size == 0 {
            break;
        }
        if pos + size > raw.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated chunk",
            ));
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
    Ok(out)
}

/// A simple request with no body.
pub fn request(method: &str, path: &str, host: &str, extra_headers: &[(&str, &str)]) -> Vec<u8> {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: tsnode/0.1\r\n");
    for (k, v) in extra_headers {
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s.into_bytes()
}

/// Writes a simple request with no body.
#[cfg(feature = "std")]
pub fn write_request(
    w: &mut dyn Write,
    method: &str,
    path: &str,
    host: &str,
    extra_headers: &[(&str, &str)],
) -> io::Result<()> {
    w.write_all(&request(method, path, host, extra_headers))?;
    w.flush()
}

/// Performs `GET path` on an already-connected stream and returns the body.
#[cfg(feature = "std")]
pub fn get(
    conn: &mut dyn Read,
    w: &mut dyn Write,
    host: &str,
    path: &str,
) -> io::Result<(u16, Vec<u8>)> {
    write_request(
        w,
        "GET",
        path,
        host,
        &[("Connection", "close"), ("Accept", "*/*")],
    )?;
    let head = read_response_head(conn)?;
    let body = read_body(conn, &head, 256 * 1024)?;
    Ok((head.status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_head_is_parsed_once_complete() {
        let full = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: DERP\r\n\r\nrest";
        for cut in 0..full.len() - 4 {
            assert_eq!(
                parse_response_head(&full[..cut]).map(|h| h.is_some()),
                Ok(false)
            );
        }
        let head = parse_response_head(full).unwrap().unwrap();
        assert_eq!(head.status, 101);
        assert_eq!(head.header("upgrade"), Some("DERP"));
        assert_eq!(head.leftover, b"rest");
        assert_eq!(
            parse_response_head(b"garbage\r\n\r\n").err(),
            Some(HeadError::BadStatusLine)
        );
        assert_eq!(
            parse_response_head(&[b'a'; MAX_HEAD + 1]).err(),
            Some(HeadError::TooLarge)
        );
    }
}
