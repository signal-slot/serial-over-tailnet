//! Tailscale control-plane client (`/key`, `/machine/register`, `/machine/map`)
//! over the ts2021 Noise transport.

use crate::controlconn::{self, ControlConn};
use crate::h2::{self, Body};
use crate::http1;
use crate::keys::{fmt_machine_pub, parse_prefixed, KeyPair, MACHINE_PUB_PREFIX};
use crate::net::{is_timeout, Conn, Dialer};
use crate::types::*;
use log::{info, warn};
use serde::Deserialize;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The map stream sends keep-alives roughly every minute; give it slack.
const MAP_STREAM_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Debug, Clone)]
pub struct ControlUrl {
    pub https: bool,
    pub host: String,
    pub port: Option<u16>,
}

impl ControlUrl {
    pub fn parse(url: &str) -> io::Result<Self> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control url must start with http:// or https://",
            ));
        };
        let hostport = rest.split('/').next().unwrap_or("");
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || h.ends_with(']') => (
                h.trim_matches(|c| c == '[' || c == ']').to_string(),
                p.parse::<u16>().ok(),
            ),
            _ => (hostport.to_string(), None),
        };
        if host.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control url has no host",
            ));
        }
        Ok(ControlUrl { https, host, port })
    }

    fn api_port(&self) -> u16 {
        self.port.unwrap_or(if self.https { 443 } else { 80 })
    }

    /// Host header / HTTP/2 authority value.
    pub fn authority(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{}", self.host, p),
            None => self.host.clone(),
        }
    }
}

#[derive(Deserialize)]
struct KeyResponse {
    #[serde(rename = "publicKey", default)]
    public_key: String,
}

pub struct ControlClient {
    dialer: Arc<dyn Dialer>,
    url: ControlUrl,
    control_pub: [u8; 32],
    machine: KeyPair,
}

impl ControlClient {
    /// Fetches the server's Noise public key and prepares a client.
    pub fn connect(
        dialer: Arc<dyn Dialer>,
        control_url: &str,
        machine: KeyPair,
    ) -> io::Result<Self> {
        let url = ControlUrl::parse(control_url)?;
        let control_pub = fetch_server_key(dialer.as_ref(), &url)?;
        info!(
            "control: server key {}",
            &fmt_machine_pub(&control_pub)[..21]
        );
        Ok(ControlClient {
            dialer,
            url,
            control_pub,
            machine,
        })
    }

    pub fn url(&self) -> &ControlUrl {
        &self.url
    }

    /// Opens a ts2021 connection, ready for HTTP/2 requests.
    fn dial(&self) -> io::Result<ControlIo> {
        let conn = self.dial_transport()?;
        let cc = ControlConn::new(
            &self.url.authority(),
            self.machine.secret(),
            &self.control_pub,
            CAPABILITY_VERSION,
        );
        ControlIo::open(conn, cc)
    }

    fn dial_transport(&self) -> io::Result<Box<dyn Conn>> {
        // The official control plane accepts the Noise upgrade on plain :80 as
        // well as behind TLS on :443. Prefer :80 (no TLS cost on the device) and
        // fall back to TLS on the API port.
        if self.url.https && self.url.port.is_none() {
            match self.dialer.dial_tcp(&self.url.host, 80) {
                Ok(c) => return Ok(c),
                Err(e) => warn!("control: plain :80 dial failed ({e}); trying TLS"),
            }
        }
        if self.url.https {
            self.dialer
                .dial_tls(&self.url.host, None, self.url.api_port())
        } else {
            self.dialer.dial_tcp(&self.url.host, self.url.api_port())
        }
    }

    fn post_json<T: serde::Serialize>(
        &self,
        h2: &mut ControlIo,
        path: &str,
        body: &T,
    ) -> io::Result<u32> {
        let json = serde_json::to_vec(body)?;
        h2.request("POST", path, &[("content-type", "application/json")], &json)
    }

    pub fn register(&self, req: &RegisterRequest) -> io::Result<RegisterResponse> {
        let mut h2 = self.dial()?;
        let id = self.post_json(&mut h2, "/machine/register", req)?;
        let status = h2.response_status(id, REQUEST_TIMEOUT)?;
        let body = h2.read_full_body(id, REQUEST_TIMEOUT, 1 << 20)?;
        if status != 200 {
            return Err(io::Error::other(format!(
                "register: HTTP {status}: {}",
                String::from_utf8_lossy(&body[..body.len().min(300)])
            )));
        }
        let resp: RegisterResponse = serde_json::from_slice(&body).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("register: bad json: {e}"),
            )
        })?;
        Ok(resp)
    }

    /// Starts a long-poll map stream (`Stream: true`).
    pub fn map_stream(&self, req: &MapRequest) -> io::Result<MapStream> {
        let mut h2 = self.dial()?;
        let id = self.post_json(&mut h2, "/machine/map", req)?;
        let status = h2.response_status(id, REQUEST_TIMEOUT)?;
        if status != 200 {
            let body = h2
                .read_full_body(id, REQUEST_TIMEOUT, 65536)
                .unwrap_or_default();
            return Err(io::Error::other(format!(
                "map: HTTP {status}: {}",
                String::from_utf8_lossy(&body[..body.len().min(300)])
            )));
        }
        Ok(MapStream {
            src: H2Body { h2, id },
            buf: Vec::new(),
            done: false,
        })
    }

    /// A single non-streaming map request, used to push endpoint updates.
    pub fn map_once(&self, req: &MapRequest) -> io::Result<MapResponse> {
        let mut req = req.clone();
        req.stream = false;
        let mut stream = self.map_stream(&req)?;
        match stream.next(REQUEST_TIMEOUT)? {
            Some(r) => Ok(r),
            // A lite (OmitPeers) update is answered with an empty body.
            None => Ok(MapResponse::default()),
        }
    }
}

/// Fetches the control server's Noise public key from `/key`.
pub fn fetch_server_key(dialer: &dyn Dialer, url: &ControlUrl) -> io::Result<[u8; 32]> {
    let mut conn = if url.https {
        dialer.dial_tls(&url.host, None, url.api_port())?
    } else {
        dialer.dial_tcp(&url.host, url.api_port())?
    };
    conn.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    let path = format!("/key?v={CAPABILITY_VERSION}");
    let host = url.authority();
    http1::write_request(
        conn.as_mut(),
        "GET",
        &path,
        &host,
        &[("Connection", "close"), ("Accept", "application/json")],
    )?;
    let head = http1::read_response_head(conn.as_mut())?;
    let body = http1::read_body(conn.as_mut(), &head, 65536)?;
    if head.status != 200 {
        return Err(io::Error::other(format!("/key: HTTP {}", head.status)));
    }
    let kr: KeyResponse = serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("/key: bad json: {e}")))?;
    parse_prefixed(MACHINE_PUB_PREFIX, &kr.public_key)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "/key: missing publicKey"))
}

/// A streaming `/machine/map` response: a sequence of little-endian
/// length-prefixed JSON messages.
pub struct MapStream<S = H2Body> {
    src: S,
    buf: Vec<u8>,
    done: bool,
}

/// Where the bytes of a map stream come from, a piece at a time.
pub trait Chunks {
    /// The next piece, `Ok(None)` at the end. Blocks up to `timeout`.
    fn chunk(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>>;
}

/// The body of an HTTP/2 response.
pub struct H2Body {
    h2: ControlIo,
    id: u32,
}

impl Chunks for H2Body {
    fn chunk(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        self.h2.read_body(self.id, timeout)
    }
}

impl<S: Chunks> MapStream<S> {
    /// Returns the next `MapResponse`, `Ok(None)` when the server ends the
    /// stream. Blocks up to `timeout` (or the built-in keep-alive timeout)
    /// for a message to begin.
    ///
    /// A message is parsed while it arrives and is never held as a whole:
    /// the first one lists every peer and every DERP region and can be far
    /// larger than what a small device has free.
    pub fn next(&mut self, timeout: Duration) -> io::Result<Option<MapResponse>> {
        let timeout = timeout.min(MAP_STREAM_TIMEOUT);
        // Every message is `u32 LE length` + JSON, streaming or not. Nothing
        // of a message is consumed until its length is complete, so a
        // timeout here leaves the stream where it was.
        while self.buf.len() < 4 {
            if self.done {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "map stream ended inside a message",
                    ))
                };
            }
            match self.src.chunk(timeout)? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => self.done = true,
            }
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > 8 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "map message too large",
            ));
        }
        self.buf.drain(..4);
        let mut body = MessageReader {
            stream: self,
            left: len,
            pos: 0,
        };
        let msg = serde_json::from_reader(&mut body);
        // Whatever the parser left unread of this message (after an error,
        // or trailing white space) is skipped, so the next message starts at
        // its length.
        let skipped = body.skip_rest();
        let msg = msg.map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("map: bad json: {e}"))
        })?;
        skipped?;
        Ok(Some(msg))
    }
}

/// The bytes of one map message: what is already in the stream's buffer,
/// then what the HTTP/2 stream delivers, up to the message's length.
struct MessageReader<'a, S> {
    stream: &'a mut MapStream<S>,
    /// Bytes of the message not yet handed out.
    left: usize,
    /// How much of `stream.buf` has been handed out.
    pos: usize,
}

impl<S: Chunks> MessageReader<'_, S> {
    /// Makes `stream.buf[pos..]` non-empty unless the message is finished.
    fn fill(&mut self) -> io::Result<()> {
        while self.left > 0 && self.pos >= self.stream.buf.len() {
            self.stream.buf.clear();
            self.pos = 0;
            if self.stream.done {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "map stream ended inside a message",
                ));
            }
            // Inside a message the server is sending: a pause this long is
            // a dead connection, and the stream cannot be resumed after it.
            match self.stream.src.chunk(REQUEST_TIMEOUT) {
                Ok(Some(chunk)) => self.stream.buf = chunk,
                Ok(None) => self.stream.done = true,
                Err(e) => {
                    self.stream.done = true;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Drops the rest of the message and gives the buffer back with only
    /// what follows the message in it.
    fn skip_rest(mut self) -> io::Result<()> {
        let mut result = Ok(());
        while self.left > 0 {
            if let Err(e) = self.fill() {
                result = Err(e);
                break;
            }
            let n = (self.stream.buf.len() - self.pos).min(self.left);
            self.pos += n;
            self.left -= n;
        }
        let pos = self.pos.min(self.stream.buf.len());
        self.stream.buf.drain(..pos);
        result
    }
}

impl<S: Chunks> io::Read for MessageReader<'_, S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || out.is_empty() {
            return Ok(0);
        }
        self.fill()?;
        let n = (self.stream.buf.len() - self.pos)
            .min(self.left)
            .min(out.len());
        out[..n].copy_from_slice(&self.stream.buf[self.pos..self.pos + n]);
        self.pos += n;
        self.left -= n;
        Ok(n)
    }
}

/// A [`ControlConn`] over a blocking connection.
pub struct ControlIo {
    conn: Box<dyn Conn>,
    cc: ControlConn,
}

fn io_error(e: controlconn::Error) -> io::Error {
    use crate::controlbase::Error as N;
    let kind = match &e {
        controlconn::Error::Noise(N::UpgradeFailed(..)) => io::ErrorKind::ConnectionRefused,
        controlconn::Error::Noise(N::Rejected(_) | N::ResponseAuth) => {
            io::ErrorKind::PermissionDenied
        }
        controlconn::Error::Noise(N::Server(_)) => io::ErrorKind::Other,
        controlconn::Error::H2(h2::Error::Goaway(_)) => io::ErrorKind::ConnectionAborted,
        controlconn::Error::H2(h2::Error::StreamReset(_)) => io::ErrorKind::ConnectionReset,
        controlconn::Error::H2(h2::Error::UnknownStream) => io::ErrorKind::NotFound,
        controlconn::Error::H2(h2::Error::EndedWithoutHeaders) => io::ErrorKind::UnexpectedEof,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, e)
}

impl ControlIo {
    /// Runs the upgrade and the handshake on `conn`.
    pub fn open(conn: Box<dyn Conn>, cc: ControlConn) -> io::Result<Self> {
        let mut io = ControlIo { conn, cc };
        io.flush()?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        while !io.cc.is_ready() {
            io.process(deadline.saturating_duration_since(Instant::now()))?;
        }
        io.flush()?;
        Ok(io)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.cc.has_output() {
            self.conn.write_all(&self.cc.take_output())?;
            self.conn.flush()?;
        }
        Ok(())
    }

    /// Reads once from the connection, blocking at most `timeout`, and
    /// processes what came.
    fn process(&mut self, timeout: Duration) -> io::Result<()> {
        self.conn
            .set_read_timeout(Some(timeout.max(Duration::from_millis(10))))?;
        let want = self.cc.wanted(2048);
        let result = self.conn.read(self.cc.read_buffer(want));
        let n = match result {
            Ok(0) => {
                let _ = self.cc.received(0);
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control connection closed",
                ));
            }
            Ok(n) => n,
            Err(e) => {
                let _ = self.cc.received(0);
                if is_timeout(&e) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "control read timeout",
                    ));
                }
                return Err(e);
            }
        };
        self.cc.received(n).map_err(io_error)?;
        self.flush()
    }

    /// Sends a complete request and returns the stream id.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<u32> {
        let id = self
            .cc
            .request(method, path, headers, body)
            .map_err(io_error)?;
        self.flush()?;
        while self.cc.body_unsent(id) {
            // Wait for WINDOW_UPDATE.
            self.process(REQUEST_TIMEOUT)?;
            self.cc.status(id).map_err(io_error)?;
        }
        Ok(id)
    }

    /// Waits for the response headers of `id` and returns the status code.
    pub fn response_status(&mut self, id: u32, timeout: Duration) -> io::Result<u16> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(st) = self.cc.status(id).map_err(io_error)? {
                return Ok(st);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timeout waiting for response headers",
                ));
            }
            self.process(left)?;
        }
    }

    /// Returns the next available body bytes, `Ok(None)` at end of stream.
    /// Blocks up to `timeout` for new data.
    pub fn read_body(&mut self, id: u32, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.cc.take_body(id).map_err(io_error)? {
                Body::Data(d) => return Ok(Some(d)),
                Body::End => return Ok(None),
                Body::Pending => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timeout waiting for body",
                ));
            }
            self.process(left)?;
        }
    }

    /// Reads the whole body.
    pub fn read_full_body(
        &mut self,
        id: u32,
        timeout: Duration,
        max: usize,
    ) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = self.read_body(id, timeout)? {
            out.extend_from_slice(&chunk);
            if out.len() > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "response body too large",
                ));
            }
        }
        self.cc.forget_stream(id);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map stream fed from prepared pieces; `None` is a timeout.
    struct Pieces(std::collections::VecDeque<Option<Vec<u8>>>);

    impl Chunks for Pieces {
        fn chunk(&mut self, _timeout: Duration) -> io::Result<Option<Vec<u8>>> {
            match self.0.pop_front() {
                Some(Some(piece)) => Ok(Some(piece)),
                Some(None) => Err(io::Error::new(io::ErrorKind::TimedOut, "timeout")),
                None => Ok(None),
            }
        }
    }

    fn stream(pieces: Vec<Option<Vec<u8>>>) -> MapStream<Pieces> {
        MapStream {
            src: Pieces(pieces.into()),
            buf: Vec::new(),
            done: false,
        }
    }

    fn message(json: &str) -> Vec<u8> {
        let mut m = (json.len() as u32).to_le_bytes().to_vec();
        m.extend_from_slice(json.as_bytes());
        m
    }

    #[test]
    fn map_messages_are_read_across_any_split() {
        let first = r#"{"Seq":1,"Domain":"example.ts.net","Peers":[{"ID":7,"Name":"a."},{"ID":8,"Name":"b."}]}"#;
        let second = r#"{"Seq":2,"KeepAlive":true}"#;
        let mut all = message(first);
        all.extend(message(second));
        // Every way of cutting the two messages into two pieces, and byte by byte.
        let mut splits: Vec<Vec<Option<Vec<u8>>>> = (0..=all.len())
            .map(|i| vec![Some(all[..i].to_vec()), Some(all[i..].to_vec())])
            .collect();
        splits.push(all.iter().map(|b| Some(vec![*b])).collect());
        for pieces in splits {
            let mut s = stream(
                pieces
                    .into_iter()
                    .filter(|p| p.as_ref().is_some_and(|p| !p.is_empty()))
                    .collect(),
            );
            let a = s.next(Duration::from_secs(1)).unwrap().unwrap();
            assert_eq!((a.seq, a.domain.as_str()), (1, "example.ts.net"));
            assert_eq!(
                a.peers.unwrap().iter().map(|p| p.id).collect::<Vec<_>>(),
                [7, 8]
            );
            let b = s.next(Duration::from_secs(1)).unwrap().unwrap();
            assert_eq!((b.seq, b.keep_alive), (2, true));
            assert!(s.next(Duration::from_secs(1)).unwrap().is_none());
        }
    }

    #[test]
    fn a_timeout_before_a_message_leaves_the_stream_usable() {
        let m = message(r#"{"Seq":5}"#);
        let mut s = stream(vec![
            None,
            Some(m[..2].to_vec()),
            None,
            Some(m[2..].to_vec()),
        ]);
        for _ in 0..2 {
            let e = s.next(Duration::from_secs(1)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        }
        assert_eq!(s.next(Duration::from_secs(1)).unwrap().unwrap().seq, 5);
    }

    #[test]
    fn a_bad_message_is_skipped_and_the_next_one_is_read() {
        let mut all = message(r#"{"Seq":"not a number"} trailing"#);
        all.extend(message(r#"{"Seq":9}"#));
        let mut s = stream(vec![Some(all)]);
        let e = s.next(Duration::from_secs(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(s.next(Duration::from_secs(1)).unwrap().unwrap().seq, 9);
    }

    #[test]
    fn a_stream_that_ends_inside_a_message_is_an_error() {
        let m = message(r#"{"Seq":5}"#);
        let mut s = stream(vec![Some(m[..7].to_vec())]);
        assert!(s.next(Duration::from_secs(1)).is_err());
        let mut s = stream(vec![Some(m[..2].to_vec())]);
        assert!(s.next(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn parses_control_urls() {
        let u = ControlUrl::parse("https://controlplane.tailscale.com").unwrap();
        assert!(u.https);
        assert_eq!(u.host, "controlplane.tailscale.com");
        assert_eq!(u.port, None);
        assert_eq!(u.authority(), "controlplane.tailscale.com");
        let u = ControlUrl::parse("http://headscale.local:8080/").unwrap();
        assert!(!u.https);
        assert_eq!(u.port, Some(8080));
        assert_eq!(u.authority(), "headscale.local:8080");
    }
}
