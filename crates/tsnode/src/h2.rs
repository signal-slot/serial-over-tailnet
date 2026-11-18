//! A deliberately small HTTP/2 client: one connection, a handful of sequential
//! streams, no server push, no priorities. It is what the Tailscale control
//! server speaks inside the ts2021 Noise channel.
//!
//! [`H2Session`] is the protocol without I/O: frames, flow control and
//! headers; bytes go in with [`H2Session::feed`] and come out of
//! [`H2Session::take_output`]. [`crate::controlconn`] runs it inside the
//! Noise records of a control connection.

use crate::hpack;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

const FRAME_DATA: u8 = 0;
const FRAME_HEADERS: u8 = 1;
const FRAME_PRIORITY: u8 = 2;
const FRAME_RST_STREAM: u8 = 3;
const FRAME_SETTINGS: u8 = 4;
const FRAME_PUSH_PROMISE: u8 = 5;
const FRAME_PING: u8 = 6;
const FRAME_GOAWAY: u8 = 7;
const FRAME_WINDOW_UPDATE: u8 = 8;
const FRAME_CONTINUATION: u8 = 9;

const FLAG_END_STREAM: u8 = 0x1;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;

const SETTINGS_HEADER_TABLE_SIZE: u16 = 1;
const SETTINGS_ENABLE_PUSH: u16 = 2;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 3;
const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 4;
const SETTINGS_MAX_FRAME_SIZE: u16 = 5;
/// (type, flags, stream id, payload)
type Frame = (u8, u8, u32, Vec<u8>);
/// What we advertise in SETTINGS_MAX_FRAME_SIZE; peers must not exceed it.
const MAX_FRAME_SIZE: usize = 16_384;
/// Cap on a stream's accumulated header block (HEADERS + CONTINUATION).
const MAX_HEADER_BLOCK: usize = 64 * 1024;
/// The HPACK dynamic table we let the server use (SETTINGS_HEADER_TABLE_SIZE).
const HEADER_TABLE_SIZE: usize = 4096;

const DEFAULT_WINDOW: u32 = 65_535;
/// Window we advertise per stream and add to the connection. With
/// `small-ram` it is small, so that the server's DATA frames stay small: a
/// frame is held whole while it is decrypted and handed on, and 16 KB frames
/// (the most a peer may send) did not fit an ESP32-C3 next to the network
/// map being parsed. The connection window cannot go below the default, so
/// the stream window is what limits the frames.
const OUR_WINDOW: u32 = if cfg!(feature = "small-ram") {
    4 * 1024
} else {
    256 * 1024
};
const WINDOW_UPDATE_THRESHOLD: u32 = OUR_WINDOW / 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The server sent GOAWAY (code and debug text).
    Goaway(String),
    StreamReset(u32),
    UnknownStream,
    EndedWithoutHeaders,
    FrameTooLarge(usize),
    HeaderBlockTooLarge,
    Hpack(hpack::Error),
    BadPadding,
    ShortPriority,
    PushPromise,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Goaway(g) => write!(f, "h2 goaway: {g}"),
            Error::StreamReset(r) => write!(f, "stream reset {r}"),
            Error::UnknownStream => write!(f, "unknown stream"),
            Error::EndedWithoutHeaders => write!(f, "stream ended without headers"),
            Error::FrameTooLarge(n) => {
                write!(f, "h2 frame of {n} bytes exceeds the advertised maximum")
            }
            Error::HeaderBlockTooLarge => write!(f, "h2 header block too large"),
            Error::Hpack(e) => write!(f, "hpack: {e}"),
            Error::BadPadding => write!(f, "bad padding"),
            Error::ShortPriority => write!(f, "short priority headers"),
            Error::PushPromise => write!(f, "unexpected push promise"),
        }
    }
}

impl core::error::Error for Error {}

/// What a stream's body has to offer.
#[derive(Debug, PartialEq, Eq)]
pub enum Body {
    Data(Vec<u8>),
    /// Nothing yet; more may come.
    Pending,
    End,
}

#[derive(Default)]
struct Stream {
    status: Option<u16>,
    headers: Vec<(String, String)>,
    header_block: Vec<u8>,
    body: Vec<u8>,
    end_stream: bool,
    reset: Option<u32>,
    send_window: i64,
    recv_consumed: u32,
    /// Request body not sent yet for lack of flow-control window.
    unsent: Vec<u8>,
}

/// The HTTP/2 protocol of one connection, without I/O.
pub struct H2Session {
    inbuf: Vec<u8>,
    out: Vec<u8>,
    decoder: hpack::Decoder,
    next_stream_id: u32,
    streams: BTreeMap<u32, Stream>,
    conn_send_window: i64,
    conn_recv_consumed: u32,
    peer_max_frame: usize,
    peer_initial_window: u32,
    goaway: Option<String>,
    authority: String,
}

impl H2Session {
    /// Starts a session; the client preface and our settings are the first
    /// output. `prefix` holds bytes already received that belong to it.
    pub fn new(authority: &str, prefix: Vec<u8>) -> Self {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(PREFACE);
        let mut settings = Vec::new();
        for (id, val) in [
            (SETTINGS_ENABLE_PUSH, 0u32),
            (SETTINGS_MAX_CONCURRENT_STREAMS, 8),
            (SETTINGS_INITIAL_WINDOW_SIZE, OUR_WINDOW),
            (SETTINGS_MAX_FRAME_SIZE, MAX_FRAME_SIZE as u32),
            (SETTINGS_HEADER_TABLE_SIZE, HEADER_TABLE_SIZE as u32),
        ] {
            settings.extend_from_slice(&id.to_be_bytes());
            settings.extend_from_slice(&val.to_be_bytes());
        }
        push_frame(&mut out, FRAME_SETTINGS, 0, 0, &settings);
        // Grow the connection-level receive window.
        if OUR_WINDOW > DEFAULT_WINDOW {
            push_frame(
                &mut out,
                FRAME_WINDOW_UPDATE,
                0,
                0,
                &(OUR_WINDOW - DEFAULT_WINDOW).to_be_bytes(),
            );
        }
        H2Session {
            inbuf: prefix,
            out,
            decoder: hpack::Decoder::new(HEADER_TABLE_SIZE),
            next_stream_id: 1,
            streams: BTreeMap::new(),
            conn_send_window: DEFAULT_WINDOW as i64,
            conn_recv_consumed: 0,
            peer_max_frame: 16_384,
            peer_initial_window: DEFAULT_WINDOW,
            goaway: None,
            authority: authority.to_string(),
        }
    }

    /// Bytes waiting to be sent to the server.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    /// Takes bytes from the server and processes every complete frame.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        self.inbuf.extend_from_slice(data);
        while let Some((typ, flags, sid, payload)) = self.take_frame()? {
            self.handle_frame(typ, flags, sid, payload)?;
        }
        self.send_unsent();
        Ok(())
    }

    /// Queues a complete request and returns its stream id. A body larger
    /// than the server's window goes out as the window opens.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<u32, Error> {
        if let Some(g) = &self.goaway {
            return Err(Error::Goaway(g.clone()));
        }
        let id = self.next_stream_id;
        self.next_stream_id += 2;
        self.streams.insert(
            id,
            Stream {
                send_window: self.peer_initial_window as i64,
                unsent: body.to_vec(),
                ..Default::default()
            },
        );

        let mut block = Vec::new();
        for (k, v) in [
            (":method", method),
            (":scheme", "https"),
            (":authority", self.authority.as_str()),
            (":path", path),
            ("user-agent", "tsnode/0.1"),
        ] {
            hpack::encode_literal(&mut block, k.as_bytes(), v.as_bytes());
        }
        for (k, v) in headers {
            hpack::encode_literal(&mut block, k.to_ascii_lowercase().as_bytes(), v.as_bytes());
        }
        if !body.is_empty() {
            hpack::encode_literal(
                &mut block,
                b"content-length",
                body.len().to_string().as_bytes(),
            );
        }
        let mut flags = FLAG_END_HEADERS;
        if body.is_empty() {
            flags |= FLAG_END_STREAM;
        }
        push_frame(&mut self.out, FRAME_HEADERS, flags, id, &block);
        self.send_unsent();
        Ok(id)
    }

    /// True while part of the request body of `id` waits for window.
    pub fn body_unsent(&self, id: u32) -> bool {
        self.streams.get(&id).is_some_and(|s| !s.unsent.is_empty())
    }

    /// Sends what the flow-control windows allow of every unsent body.
    fn send_unsent(&mut self) {
        for (&id, s) in self.streams.iter_mut() {
            while !s.unsent.is_empty() && s.reset.is_none() {
                let allowed = s.send_window.min(self.conn_send_window).max(0) as usize;
                if allowed == 0 {
                    break;
                }
                let n = s.unsent.len().min(allowed).min(self.peer_max_frame);
                let last = n == s.unsent.len();
                push_frame(
                    &mut self.out,
                    FRAME_DATA,
                    if last { FLAG_END_STREAM } else { 0 },
                    id,
                    &s.unsent[..n],
                );
                s.unsent.drain(..n);
                self.conn_send_window -= n as i64;
                s.send_window -= n as i64;
            }
        }
    }

    fn stream(&self, id: u32) -> Result<&Stream, Error> {
        let s = self.streams.get(&id).ok_or(Error::UnknownStream)?;
        if let Some(r) = s.reset {
            return Err(Error::StreamReset(r));
        }
        Ok(s)
    }

    /// The response status of `id`, once its headers are in.
    pub fn status(&self, id: u32) -> Result<Option<u16>, Error> {
        let s = self.stream(id)?;
        if let Some(st) = s.status {
            return Ok(Some(st));
        }
        if s.end_stream {
            return Err(Error::EndedWithoutHeaders);
        }
        self.check_goaway()?;
        Ok(None)
    }

    pub fn response_header(&self, id: u32, name: &str) -> Option<&str> {
        self.streams
            .get(&id)?
            .headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body bytes of `id` received so far.
    pub fn take_body(&mut self, id: u32) -> Result<Body, Error> {
        self.stream(id)?;
        let s = self.streams.get_mut(&id).unwrap();
        if !s.body.is_empty() {
            return Ok(Body::Data(core::mem::take(&mut s.body)));
        }
        if s.end_stream {
            return Ok(Body::End);
        }
        self.check_goaway()?;
        Ok(Body::Pending)
    }

    pub fn forget_stream(&mut self, id: u32) {
        self.streams.remove(&id);
    }

    fn check_goaway(&self) -> Result<(), Error> {
        match &self.goaway {
            Some(g) => Err(Error::Goaway(g.clone())),
            None => Ok(()),
        }
    }

    fn take_frame(&mut self) -> Result<Option<Frame>, Error> {
        if self.inbuf.len() < 9 {
            return Ok(None);
        }
        let len = ((self.inbuf[0] as usize) << 16)
            | ((self.inbuf[1] as usize) << 8)
            | self.inbuf[2] as usize;
        if len > MAX_FRAME_SIZE {
            // Larger than what we advertised in SETTINGS: a protocol error,
            // and on a small device an allocation we must not attempt.
            return Err(Error::FrameTooLarge(len));
        }
        if self.inbuf.len() < 9 + len {
            return Ok(None);
        }
        let typ = self.inbuf[3];
        let flags = self.inbuf[4];
        let sid = u32::from_be_bytes([self.inbuf[5], self.inbuf[6], self.inbuf[7], self.inbuf[8]])
            & 0x7fff_ffff;
        let payload = self.inbuf[9..9 + len].to_vec();
        self.inbuf.drain(..9 + len);
        Ok(Some((typ, flags, sid, payload)))
    }

    fn handle_frame(
        &mut self,
        typ: u8,
        flags: u8,
        sid: u32,
        payload: Vec<u8>,
    ) -> Result<(), Error> {
        match typ {
            FRAME_DATA => {
                let data = strip_padding(&payload, flags & FLAG_PADDED != 0)?;
                let len = payload.len() as u32;
                if let Some(s) = self.streams.get_mut(&sid) {
                    s.body.extend_from_slice(data);
                    if flags & FLAG_END_STREAM != 0 {
                        s.end_stream = true;
                    }
                    s.recv_consumed += len;
                }
                self.conn_recv_consumed += len;
                self.maybe_window_update(sid);
            }
            FRAME_HEADERS => {
                let mut block = strip_padding(&payload, flags & FLAG_PADDED != 0)?;
                if flags & FLAG_PRIORITY != 0 {
                    if block.len() < 5 {
                        return Err(Error::ShortPriority);
                    }
                    block = &block[5..];
                }
                let s = self.streams.entry(sid).or_default();
                s.header_block.extend_from_slice(block);
                if flags & FLAG_END_STREAM != 0 {
                    s.end_stream = true;
                }
                if flags & FLAG_END_HEADERS != 0 {
                    self.finish_headers(sid)?;
                }
            }
            FRAME_CONTINUATION => {
                let s = self.streams.entry(sid).or_default();
                s.header_block.extend_from_slice(&payload);
                if flags & FLAG_END_HEADERS != 0 {
                    self.finish_headers(sid)?;
                }
            }
            FRAME_PRIORITY => {}
            FRAME_RST_STREAM => {
                let code = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else {
                    0
                };
                if let Some(s) = self.streams.get_mut(&sid) {
                    s.reset = Some(code);
                }
            }
            FRAME_SETTINGS => {
                if flags & FLAG_ACK == 0 {
                    for chunk in payload.as_chunks::<6>().0 {
                        let id = u16::from_be_bytes([chunk[0], chunk[1]]);
                        let val = u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]);
                        match id {
                            SETTINGS_MAX_FRAME_SIZE => {
                                self.peer_max_frame = (val as usize).clamp(16_384, 1 << 24)
                            }
                            SETTINGS_INITIAL_WINDOW_SIZE => {
                                let delta = val as i64 - self.peer_initial_window as i64;
                                self.peer_initial_window = val;
                                for s in self.streams.values_mut() {
                                    s.send_window += delta;
                                }
                            }
                            // Our encoder keeps no table (literals without
                            // indexing), so any size the peer allows is fine.
                            SETTINGS_HEADER_TABLE_SIZE => {}
                            _ => {}
                        }
                    }
                    push_frame(&mut self.out, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
                }
            }
            FRAME_PUSH_PROMISE => return Err(Error::PushPromise),
            FRAME_PING => {
                if flags & FLAG_ACK == 0 {
                    push_frame(&mut self.out, FRAME_PING, FLAG_ACK, 0, &payload);
                }
            }
            FRAME_GOAWAY => {
                let code = if payload.len() >= 8 {
                    u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]])
                } else {
                    0
                };
                let debug = if payload.len() > 8 {
                    String::from_utf8_lossy(&payload[8..]).into_owned()
                } else {
                    String::new()
                };
                self.goaway = Some(alloc::format!("code {code} {debug}"));
            }
            FRAME_WINDOW_UPDATE if payload.len() >= 4 => {
                let inc = (u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                    & 0x7fff_ffff) as i64;
                if sid == 0 {
                    self.conn_send_window += inc;
                } else if let Some(s) = self.streams.get_mut(&sid) {
                    s.send_window += inc;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish_headers(&mut self, sid: u32) -> Result<(), Error> {
        let block = match self.streams.get_mut(&sid) {
            Some(s) => core::mem::take(&mut s.header_block),
            None => return Ok(()),
        };
        if block.len() > MAX_HEADER_BLOCK {
            return Err(Error::HeaderBlockTooLarge);
        }
        let decoded = self.decoder.decode(&block).map_err(Error::Hpack)?;
        let s = self.streams.get_mut(&sid).unwrap();
        for (k, v) in decoded {
            let k = String::from_utf8_lossy(&k).into_owned();
            let v = String::from_utf8_lossy(&v).into_owned();
            if k == ":status" {
                if let Ok(code) = v.parse::<u16>() {
                    // Ignore 1xx informational responses.
                    if code >= 200 || s.status.is_none() {
                        s.status = Some(code);
                    }
                }
            } else {
                s.headers.push((k, v));
            }
        }
        Ok(())
    }

    fn maybe_window_update(&mut self, sid: u32) {
        if self.conn_recv_consumed >= WINDOW_UPDATE_THRESHOLD {
            push_frame(
                &mut self.out,
                FRAME_WINDOW_UPDATE,
                0,
                0,
                &self.conn_recv_consumed.to_be_bytes(),
            );
            self.conn_recv_consumed = 0;
        }
        if let Some(s) = self.streams.get_mut(&sid) {
            if s.recv_consumed >= WINDOW_UPDATE_THRESHOLD && !s.end_stream {
                push_frame(
                    &mut self.out,
                    FRAME_WINDOW_UPDATE,
                    0,
                    sid,
                    &s.recv_consumed.to_be_bytes(),
                );
                s.recv_consumed = 0;
            }
        }
    }
}

fn strip_padding(payload: &[u8], padded: bool) -> Result<&[u8], Error> {
    if !padded {
        return Ok(payload);
    }
    let pad = *payload.first().ok_or(Error::BadPadding)? as usize;
    if pad + 1 > payload.len() {
        return Err(Error::BadPadding);
    }
    Ok(&payload[1..payload.len() - pad])
}

fn push_frame(out: &mut Vec<u8>, typ: u8, flags: u8, sid: u32, payload: &[u8]) {
    let len = payload.len();
    out.push((len >> 16) as u8);
    out.push((len >> 8) as u8);
    out.push(len as u8);
    out.push(typ);
    out.push(flags);
    out.extend_from_slice(&sid.to_be_bytes());
    out.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(typ: u8, flags: u8, sid: u32, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        push_frame(&mut f, typ, flags, sid, payload);
        f
    }

    /// Splits output into frames (after the preface, if present).
    fn frames(out: &[u8]) -> Vec<Frame> {
        let mut rest = out.strip_prefix(PREFACE).unwrap_or(out);
        let mut v = Vec::new();
        while !rest.is_empty() {
            let len = ((rest[0] as usize) << 16) | ((rest[1] as usize) << 8) | rest[2] as usize;
            let sid = u32::from_be_bytes(rest[5..9].try_into().unwrap());
            v.push((rest[3], rest[4], sid, rest[9..9 + len].to_vec()));
            rest = &rest[9 + len..];
        }
        v
    }

    fn setting(id: u16, val: u32) -> Vec<u8> {
        let mut s = id.to_be_bytes().to_vec();
        s.extend_from_slice(&val.to_be_bytes());
        s
    }

    #[test]
    fn a_request_and_its_response() {
        let mut s = H2Session::new("example.com", Vec::new());
        let start = s.take_output();
        assert!(start.starts_with(PREFACE));
        assert_eq!(frames(&start)[0].0, FRAME_SETTINGS);

        let id = s
            .request("POST", "/machine/map", &[("Content-Type", "x")], b"{}")
            .unwrap();
        assert_eq!(id, 1);
        let sent = frames(&s.take_output());
        assert_eq!(sent.len(), 2);
        assert_eq!((sent[0].0, sent[0].1), (FRAME_HEADERS, FLAG_END_HEADERS));
        let fields = hpack::Decoder::new(4096).decode(&sent[0].3).unwrap();
        assert!(fields.contains(&(b":path".to_vec(), b"/machine/map".to_vec())));
        assert!(fields.contains(&(b"content-type".to_vec(), b"x".to_vec())));
        assert!(fields.contains(&(b"content-length".to_vec(), b"2".to_vec())));
        assert_eq!(sent[1], (FRAME_DATA, FLAG_END_STREAM, 1, b"{}".to_vec()));

        assert_eq!(s.status(1), Ok(None));
        let mut input = frame(FRAME_SETTINGS, 0, 0, &[]);
        input.extend(frame(FRAME_PING, 0, 0, b"pingpong"));
        input.extend(frame(FRAME_HEADERS, FLAG_END_HEADERS, 1, &[0x88])); // :status 200
        input.extend(frame(FRAME_DATA, 0, 1, b"hel"));
        input.extend(frame(FRAME_DATA, FLAG_END_STREAM, 1, b"lo"));
        // Fed a byte at a time.
        for b in &input {
            s.feed(&[*b]).unwrap();
        }
        assert_eq!(s.status(1), Ok(Some(200)));
        assert_eq!(s.take_body(1), Ok(Body::Data(b"hello".to_vec())));
        assert_eq!(s.take_body(1), Ok(Body::End));
        let replies = frames(&s.take_output());
        assert_eq!(
            replies,
            vec![
                (FRAME_SETTINGS, FLAG_ACK, 0, vec![]),
                (FRAME_PING, FLAG_ACK, 0, b"pingpong".to_vec()),
            ]
        );
    }

    #[test]
    fn a_body_waits_for_the_window() {
        let mut s = H2Session::new("h", Vec::new());
        s.take_output();
        s.feed(&frame(
            FRAME_SETTINGS,
            0,
            0,
            &setting(SETTINGS_INITIAL_WINDOW_SIZE, 10),
        ))
        .unwrap();
        s.take_output();
        let id = s.request("POST", "/", &[], &[b'x'; 25]).unwrap();
        let sent = frames(&s.take_output());
        assert_eq!(sent[1], (FRAME_DATA, 0, id, vec![b'x'; 10]));
        assert!(s.body_unsent(id));
        s.feed(&frame(FRAME_WINDOW_UPDATE, 0, id, &20u32.to_be_bytes()))
            .unwrap();
        let sent = frames(&s.take_output());
        assert_eq!(
            sent,
            vec![(FRAME_DATA, FLAG_END_STREAM, id, vec![b'x'; 15])]
        );
        assert!(!s.body_unsent(id));
    }

    #[test]
    fn receive_windows_are_reopened() {
        let mut s = H2Session::new("h", Vec::new());
        s.take_output();
        let id = s.request("GET", "/", &[], &[]).unwrap();
        s.take_output();
        let chunk = vec![0u8; 1024];
        let mut updates = 0;
        for _ in 0..(OUR_WINDOW as usize / 1024) {
            s.feed(&frame(FRAME_DATA, 0, id, &chunk)).unwrap();
            updates += frames(&s.take_output())
                .iter()
                .filter(|f| f.0 == FRAME_WINDOW_UPDATE && f.2 == id)
                .count();
        }
        assert!(
            updates >= 1,
            "the stream window is reopened before it runs out"
        );
    }

    #[test]
    fn errors_from_the_server() {
        let mut s = H2Session::new("h", Vec::new());
        let id = s.request("GET", "/", &[], &[]).unwrap();
        s.feed(&frame(FRAME_RST_STREAM, 0, id, &8u32.to_be_bytes()))
            .unwrap();
        assert_eq!(s.status(id), Err(Error::StreamReset(8)));
        assert_eq!(s.status(99), Err(Error::UnknownStream));

        let mut s = H2Session::new("h", Vec::new());
        let id = s.request("GET", "/", &[], &[]).unwrap();
        let mut goaway = 0u32.to_be_bytes().to_vec();
        goaway.extend_from_slice(&2u32.to_be_bytes());
        goaway.extend_from_slice(b"bye");
        s.feed(&frame(FRAME_GOAWAY, 0, 0, &goaway)).unwrap();
        assert_eq!(s.status(id), Err(Error::Goaway("code 2 bye".into())));
        assert!(s.request("GET", "/", &[], &[]).is_err());

        let mut s = H2Session::new("h", Vec::new());
        assert_eq!(
            s.feed(&[0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0]),
            Err(Error::FrameTooLarge(65536))
        );
        let mut s = H2Session::new("h", Vec::new());
        assert_eq!(
            s.feed(&frame(FRAME_HEADERS, FLAG_END_HEADERS, 1, &[0x80])),
            Err(Error::Hpack(hpack::Error::BadIndex(0)))
        );
        let mut s = H2Session::new("h", Vec::new());
        assert_eq!(
            s.feed(&frame(FRAME_DATA, FLAG_PADDED, 1, &[5, 1])),
            Err(Error::BadPadding)
        );
    }
}
