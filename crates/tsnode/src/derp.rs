//! DERP relay client (Tailscale's TLS-tunnelled packet relay).
//!
//! [`DerpSession`] is the protocol without I/O: bytes from the server go in
//! with [`DerpSession::feed`], bytes for the server come out of
//! [`DerpSession::take_output`]. TLS is the caller's. [`DerpClient`] runs a
//! session over a blocking [`Conn`].

use crate::crypto::{nacl_box_open, nacl_box_seal, random_array};
use crate::http1;
use crate::keys::{short, KeyPair};
use crate::net::{is_timeout, Conn, Dialer};
use crate::types::DerpNode;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::IpAddr;
use log::{debug, warn};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

/// 8-byte magic: "DERP🔑".
pub const MAGIC: &[u8; 8] = b"DERP\xf0\x9f\x94\x91";
pub const PROTOCOL_VERSION: i64 = 2;
pub const MAX_PACKET: usize = 64 << 10;
const FRAME_HEADER_LEN: usize = 5;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const MAX_FRAME: usize = MAX_PACKET + KEY_LEN + 64;

const FRAME_SERVER_KEY: u8 = 0x01;
const FRAME_CLIENT_INFO: u8 = 0x02;
const FRAME_SERVER_INFO: u8 = 0x03;
const FRAME_SEND_PACKET: u8 = 0x04;
const FRAME_RECV_PACKET: u8 = 0x05;
const FRAME_KEEP_ALIVE: u8 = 0x06;
const FRAME_NOTE_PREFERRED: u8 = 0x07;
const FRAME_PEER_GONE: u8 = 0x08;
const FRAME_PEER_PRESENT: u8 = 0x09;
const FRAME_PING: u8 = 0x12;
const FRAME_PONG: u8 = 0x13;
const FRAME_HEALTH: u8 = 0x14;
const FRAME_RESTARTING: u8 = 0x15;

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Packet { src: [u8; 32], data: Vec<u8> },
    PeerGone([u8; 32]),
    Health(String),
    Restarting,
    Pong([u8; 8]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The HTTP upgrade was answered with another status.
    UpgradeFailed(u16),
    BadHead(http1::HeadError),
    BadGreeting,
    BadServerInfo,
    ServerInfoAuth,
    FrameTooLarge,
    ShortPacket,
    PacketTooBig,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::UpgradeFailed(s) => write!(f, "derp upgrade failed: HTTP {s}"),
            Error::BadHead(e) => write!(f, "derp: {e}"),
            Error::BadGreeting => write!(f, "derp: bad server greeting"),
            Error::BadServerInfo => write!(f, "derp: bad server info"),
            Error::ServerInfoAuth => write!(f, "derp: server info authentication failed"),
            Error::FrameTooLarge => write!(f, "derp: frame too large"),
            Error::ShortPacket => write!(f, "derp: short recv packet"),
            Error::PacketTooBig => write!(f, "derp: packet too big"),
        }
    }
}

impl core::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the server's first bytes: an HTTP head (no fast start)
    /// or directly the server key frame.
    Head,
    ServerKey,
    ServerInfo,
    Ready,
}

/// The DERP protocol of one connection, without I/O.
pub struct DerpSession {
    phase: Phase,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    node_key: KeyPair,
    server_key: [u8; 32],
    server_info: Vec<u8>,
    /// Length of `inbuf` before the space handed out by `read_buffer`.
    recv_start: usize,
    /// Frames received so far, of any type (keep-alives included): a caller
    /// that tracks the connection's liveness compares it.
    pub frames_received: u64,
}

impl DerpSession {
    /// Starts a session with the server `host`; the HTTP upgrade request is
    /// the first output.
    pub fn new(host: &str, node_key: &KeyPair) -> Self {
        DerpSession {
            phase: Phase::Head,
            inbuf: Vec::new(),
            out: http1::request(
                "GET",
                "/derp",
                host,
                &[
                    ("Upgrade", "DERP"),
                    ("Connection", "Upgrade"),
                    ("Derp-Fast-Start", "1"),
                ],
            ),
            node_key: node_key.clone(),
            server_key: [0; 32],
            server_info: Vec::new(),
            recv_start: 0,
            frames_received: 0,
        }
    }

    /// True once the handshake is done and packets can be sent.
    pub fn is_ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    pub fn server_key(&self) -> &[u8; 32] {
        &self.server_key
    }

    /// The server's info JSON from the handshake.
    pub fn server_info(&self) -> &[u8] {
        &self.server_info
    }

    /// Bytes waiting to be sent to the server.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    /// Takes bytes from the server and advances the handshake. Events are
    /// read with [`poll_event`](Self::poll_event).
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        self.inbuf.extend_from_slice(data);
        self.handshake()
    }

    /// How many bytes to read next: what completes the next frame (its
    /// header first), at most `max`. Reading no more than that keeps the
    /// buffer at about one frame.
    pub fn wanted(&self, max: usize) -> usize {
        let want = if self.phase == Phase::Head {
            // An HTTP head, or a frame header: small reads either way.
            256
        } else if self.inbuf.len() < FRAME_HEADER_LEN {
            FRAME_HEADER_LEN - self.inbuf.len()
        } else {
            let len =
                u32::from_be_bytes([self.inbuf[1], self.inbuf[2], self.inbuf[3], self.inbuf[4]])
                    as usize;
            (FRAME_HEADER_LEN + len).saturating_sub(self.inbuf.len())
        };
        want.clamp(1, max)
    }

    /// Space for reading up to `size` bytes from the server straight into
    /// the session (no buffer on the caller's stack, where the handshake's
    /// cryptography also needs room); then [`received`](Self::received).
    pub fn read_buffer(&mut self, size: usize) -> &mut [u8] {
        self.recv_start = self.inbuf.len();
        self.inbuf.resize(self.recv_start + size, 0);
        &mut self.inbuf[self.recv_start..]
    }

    /// `n` bytes were read into the [`read_buffer`](Self::read_buffer);
    /// like [`feed`](Self::feed) with them.
    pub fn received(&mut self, n: usize) -> Result<(), Error> {
        self.inbuf.truncate(self.recv_start + n);
        self.handshake()
    }

    fn handshake(&mut self) -> Result<(), Error> {
        loop {
            match self.phase {
                Phase::Head => {
                    let Some(&first) = self.inbuf.first() else {
                        return Ok(());
                    };
                    // A frame starts with its type (0x01), a head with "HTTP/".
                    if first == b'H' {
                        let head = match http1::parse_response_head(&self.inbuf) {
                            Ok(Some(head)) => head,
                            Ok(None) => return Ok(()),
                            Err(e) => return Err(Error::BadHead(e)),
                        };
                        if head.status != 101 {
                            return Err(Error::UpgradeFailed(head.status));
                        }
                        self.inbuf = head.leftover;
                    }
                    self.phase = Phase::ServerKey;
                }
                Phase::ServerKey => {
                    let Some((t, payload)) = self.take_frame()? else {
                        return Ok(());
                    };
                    if t != FRAME_SERVER_KEY
                        || payload.len() < MAGIC.len() + KEY_LEN
                        || &payload[..MAGIC.len()] != MAGIC
                    {
                        return Err(Error::BadGreeting);
                    }
                    self.server_key
                        .copy_from_slice(&payload[MAGIC.len()..MAGIC.len() + KEY_LEN]);
                    // ClientInfo: our public key, a nonce, box(json).
                    let info = alloc::format!("{{\"version\":{PROTOCOL_VERSION}}}");
                    let nonce: [u8; NONCE_LEN] = random_array();
                    let sealed = nacl_box_seal(
                        self.node_key.secret(),
                        &self.server_key,
                        &nonce,
                        info.as_bytes(),
                    );
                    let mut body = Vec::with_capacity(KEY_LEN + NONCE_LEN + sealed.len());
                    body.extend_from_slice(self.node_key.public());
                    body.extend_from_slice(&nonce);
                    body.extend_from_slice(&sealed);
                    self.push_frame(FRAME_CLIENT_INFO, &body);
                    self.phase = Phase::ServerInfo;
                }
                Phase::ServerInfo => {
                    let Some((t, payload)) = self.take_frame()? else {
                        return Ok(());
                    };
                    if t != FRAME_SERVER_INFO || payload.len() < NONCE_LEN {
                        return Err(Error::BadServerInfo);
                    }
                    let nonce: [u8; NONCE_LEN] = payload[..NONCE_LEN].try_into().unwrap();
                    self.server_info = nacl_box_open(
                        self.node_key.secret(),
                        &self.server_key,
                        &nonce,
                        &payload[NONCE_LEN..],
                    )
                    .map_err(|_| Error::ServerInfoAuth)?;
                    self.phase = Phase::Ready;
                }
                Phase::Ready => return Ok(()),
            }
        }
    }

    /// The next event among the bytes fed so far. Keep-alives and the
    /// server's pings (answered here) are not events.
    pub fn poll_event(&mut self) -> Result<Option<Event>, Error> {
        if self.phase != Phase::Ready {
            return Ok(None);
        }
        while let Some((t, payload)) = self.take_frame()? {
            match t {
                FRAME_RECV_PACKET => {
                    if payload.len() < KEY_LEN {
                        return Err(Error::ShortPacket);
                    }
                    let src: [u8; 32] = payload[..KEY_LEN].try_into().unwrap();
                    return Ok(Some(Event::Packet {
                        src,
                        data: payload[KEY_LEN..].to_vec(),
                    }));
                }
                FRAME_PING => {
                    if payload.len() == 8 {
                        self.push_frame(FRAME_PONG, &payload);
                    }
                }
                FRAME_PONG => {
                    let mut d = [0u8; 8];
                    if payload.len() == 8 {
                        d.copy_from_slice(&payload);
                    }
                    return Ok(Some(Event::Pong(d)));
                }
                FRAME_PEER_GONE if payload.len() >= KEY_LEN => {
                    return Ok(Some(Event::PeerGone(
                        payload[..KEY_LEN].try_into().unwrap(),
                    )));
                }
                FRAME_HEALTH => {
                    return Ok(Some(Event::Health(
                        String::from_utf8_lossy(&payload).into_owned(),
                    )));
                }
                FRAME_RESTARTING => return Ok(Some(Event::Restarting)),
                FRAME_KEEP_ALIVE | FRAME_PEER_GONE | FRAME_PEER_PRESENT => {}
                other => warn!("derp: unknown frame type {other:#x}"),
            }
        }
        Ok(None)
    }

    /// Queues a packet for the peer `dst`.
    pub fn send_packet(&mut self, dst: &[u8; 32], pkt: &[u8]) -> Result<(), Error> {
        if pkt.len() > MAX_PACKET {
            return Err(Error::PacketTooBig);
        }
        let mut body = Vec::with_capacity(KEY_LEN + pkt.len());
        body.extend_from_slice(dst);
        body.extend_from_slice(pkt);
        self.push_frame(FRAME_SEND_PACKET, &body);
        Ok(())
    }

    pub fn note_preferred(&mut self, preferred: bool) {
        self.push_frame(FRAME_NOTE_PREFERRED, &[preferred as u8]);
    }

    pub fn ping(&mut self, data: [u8; 8]) {
        self.push_frame(FRAME_PING, &data);
    }

    fn push_frame(&mut self, t: u8, payload: &[u8]) {
        self.out.push(t);
        self.out
            .extend_from_slice(&(payload.len() as u32).to_be_bytes());
        self.out.extend_from_slice(payload);
    }

    fn take_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>, Error> {
        if self.inbuf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }
        let t = self.inbuf[0];
        let len = u32::from_be_bytes([self.inbuf[1], self.inbuf[2], self.inbuf[3], self.inbuf[4]])
            as usize;
        if len > MAX_FRAME {
            return Err(Error::FrameTooLarge);
        }
        if self.inbuf.len() < FRAME_HEADER_LEN + len {
            return Ok(None);
        }
        let payload = self.inbuf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len].to_vec();
        self.inbuf.drain(..FRAME_HEADER_LEN + len);
        self.frames_received += 1;
        Ok(Some((t, payload)))
    }
}

fn io_error(e: Error) -> io::Error {
    let kind = match e {
        Error::UpgradeFailed(_) => io::ErrorKind::ConnectionRefused,
        Error::ServerInfoAuth => io::ErrorKind::PermissionDenied,
        Error::PacketTooBig => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, e)
}

/// A [`DerpSession`] over a blocking connection.
pub struct DerpClient {
    conn: Box<dyn Conn>,
    session: DerpSession,
    region_id: i32,
    pub server_key: [u8; 32],
    pub last_activity: Instant,
    pub last_ping_sent: Option<Instant>,
}

impl DerpClient {
    /// Connects to `node` (TLS to its IPv4 address if given, else its host name),
    /// performs the DERP handshake with our node key and returns the client.
    pub fn connect(
        dialer: &dyn Dialer,
        node: &DerpNode,
        region_id: i32,
        node_key: &KeyPair,
    ) -> io::Result<Self> {
        let port = if node.derp_port > 0 {
            node.derp_port as u16
        } else {
            443
        };
        let addr: Option<IpAddr> = node.ipv4.map(IpAddr::V4);
        let host = node.host_name.clone();
        let conn = if node.insecure_for_tests {
            dialer.dial_tcp(
                addr.map(|a| a.to_string()).as_deref().unwrap_or(&host),
                port,
            )?
        } else {
            dialer.dial_tls(&host, addr, port)?
        };
        let mut client = DerpClient {
            conn,
            session: DerpSession::new(&host, node_key),
            region_id,
            server_key: [0; 32],
            last_activity: Instant::now(),
            last_ping_sent: None,
        };
        client.flush()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !client.session.is_ready() {
            let now = Instant::now();
            if now >= deadline || !client.fill(deadline - now)? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "derp: handshake timed out",
                ));
            }
            client.flush()?;
        }
        client.server_key = *client.session.server_key();
        debug!(
            "derp[{}]: connected to {} ({}), server info {}",
            region_id,
            host,
            short(&client.server_key),
            String::from_utf8_lossy(client.session.server_info())
        );
        client.last_activity = Instant::now();
        Ok(client)
    }

    pub fn region_id(&self) -> i32 {
        self.region_id
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.session.has_output() {
            self.conn.write_all(&self.session.take_output())?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub fn send_packet(&mut self, dst: &[u8; 32], pkt: &[u8]) -> io::Result<()> {
        self.session.send_packet(dst, pkt).map_err(io_error)?;
        self.flush()
    }

    pub fn note_preferred(&mut self, preferred: bool) -> io::Result<()> {
        self.session.note_preferred(preferred);
        self.flush()
    }

    pub fn ping(&mut self) -> io::Result<[u8; 8]> {
        let data: [u8; 8] = random_array();
        self.session.ping(data);
        self.flush()?;
        self.last_ping_sent = Some(Instant::now());
        Ok(data)
    }

    /// Reads more bytes into the session, blocking up to `timeout`.
    /// Returns `Ok(false)` on timeout.
    fn fill(&mut self, timeout: Duration) -> io::Result<bool> {
        self.conn
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let want = self.session.wanted(4096);
        let result = self.conn.read(self.session.read_buffer(want));
        match result {
            Ok(0) => {
                self.session.received(0).map_err(io_error)?;
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "derp: connection closed",
                ))
            }
            Ok(n) => {
                self.session.received(n).map_err(io_error)?;
                Ok(true)
            }
            Err(e) => {
                self.session.received(0).map_err(io_error)?;
                if is_timeout(&e) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Waits up to `timeout` for the next event. Keep-alives and pings are
    /// handled internally and yield `Ok(None)` like a timeout does.
    pub fn recv(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let deadline = Instant::now() + timeout;
        loop {
            let frames = self.session.frames_received;
            let event = self.session.poll_event().map_err(io_error)?;
            if self.session.frames_received != frames {
                self.last_activity = Instant::now();
            }
            self.flush()?;
            if let Some(event) = event {
                return Ok(Some(event));
            }
            if frames != self.session.frames_received {
                // Only keep-alives or pings: report them like a timeout.
                return Ok(None);
            }
            let now = Instant::now();
            if now >= deadline || !self.fill(deadline - now)? {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![t];
        f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    /// Parses the frames in `out`.
    fn frames(mut out: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut v = Vec::new();
        while !out.is_empty() {
            let len = u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
            v.push((out[0], out[5..5 + len].to_vec()));
            out = &out[5 + len..];
        }
        v
    }

    /// Runs the server side of the handshake, feeding the client one byte
    /// at a time, with or without an HTTP head first.
    fn handshake(with_head: bool) -> (DerpSession, KeyPair) {
        let client_key = KeyPair::generate();
        let server = KeyPair::generate();
        let mut s = DerpSession::new("derp.example", &client_key);
        let req = s.take_output();
        assert!(req.starts_with(b"GET /derp HTTP/1.1\r\nHost: derp.example\r\n"));

        let mut greeting = Vec::new();
        if with_head {
            greeting
                .extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: DERP\r\n\r\n");
        }
        let mut key_payload = MAGIC.to_vec();
        key_payload.extend_from_slice(server.public());
        greeting.extend(frame(FRAME_SERVER_KEY, &key_payload));
        for b in &greeting {
            s.feed(&[*b]).unwrap();
        }

        let info = frames(&s.take_output());
        assert_eq!(info.len(), 1);
        let (t, body) = &info[0];
        assert_eq!(*t, FRAME_CLIENT_INFO);
        assert_eq!(&body[..32], client_key.public());
        let nonce: [u8; 24] = body[32..56].try_into().unwrap();
        let json =
            nacl_box_open(server.secret(), client_key.public(), &nonce, &body[56..]).unwrap();
        assert_eq!(json, b"{\"version\":2}");
        assert!(!s.is_ready());

        let nonce = [7u8; 24];
        let mut si = nonce.to_vec();
        si.extend(nacl_box_seal(
            server.secret(),
            client_key.public(),
            &nonce,
            b"{}",
        ));
        for b in frame(FRAME_SERVER_INFO, &si) {
            s.feed(&[b]).unwrap();
        }
        assert!(s.is_ready());
        assert_eq!(s.server_key(), server.public());
        assert_eq!(s.server_info(), b"{}");
        (s, client_key)
    }

    #[test]
    fn handshake_with_fast_start_and_with_an_http_head() {
        handshake(false);
        handshake(true);
    }

    #[test]
    fn events_pings_and_packets() {
        let (mut s, _) = handshake(false);
        let peer = [9u8; 32];
        let mut input = frame(FRAME_KEEP_ALIVE, &[]);
        input.extend(frame(FRAME_PING, b"12345678"));
        let mut pkt = peer.to_vec();
        pkt.extend_from_slice(b"hello");
        input.extend(frame(FRAME_RECV_PACKET, &pkt));
        input.extend(frame(FRAME_PEER_GONE, &peer));
        input.extend(frame(FRAME_HEALTH, b"degraded"));
        input.extend(frame(FRAME_RESTARTING, &[]));
        // Fed in two halves: nothing is lost at the split.
        let (a, b) = input.split_at(input.len() / 2);
        s.feed(a).unwrap();
        let mut events = Vec::new();
        while let Some(e) = s.poll_event().unwrap() {
            events.push(e);
        }
        s.feed(b).unwrap();
        while let Some(e) = s.poll_event().unwrap() {
            events.push(e);
        }
        assert_eq!(
            events,
            vec![
                Event::Packet {
                    src: peer,
                    data: b"hello".to_vec()
                },
                Event::PeerGone(peer),
                Event::Health("degraded".into()),
                Event::Restarting,
            ]
        );
        assert_eq!(s.frames_received, 2 + 6);
        // The ping was answered.
        assert_eq!(
            frames(&s.take_output()),
            vec![(FRAME_PONG, b"12345678".to_vec())]
        );

        s.send_packet(&peer, b"out").unwrap();
        s.note_preferred(true);
        s.ping(*b"abcdefgh");
        let mut sent = peer.to_vec();
        sent.extend_from_slice(b"out");
        assert_eq!(
            frames(&s.take_output()),
            vec![
                (FRAME_SEND_PACKET, sent),
                (FRAME_NOTE_PREFERRED, vec![1]),
                (FRAME_PING, b"abcdefgh".to_vec()),
            ]
        );
        assert_eq!(
            s.send_packet(&peer, &vec![0; MAX_PACKET + 1]),
            Err(Error::PacketTooBig)
        );
    }

    #[test]
    fn reading_into_the_session_buffer_is_like_feeding() {
        let (mut s, _) = handshake(false);
        let peer = [4u8; 32];
        let mut pkt = peer.to_vec();
        pkt.extend_from_slice(b"via buffer");
        let input = frame(FRAME_RECV_PACKET, &pkt);
        let (a, b) = input.split_at(10);
        for part in [a, b] {
            let buf = s.read_buffer(4096);
            buf[..part.len()].copy_from_slice(part);
            s.received(part.len()).unwrap();
        }
        // A read that returned nothing leaves the buffer as it was.
        s.read_buffer(4096);
        s.received(0).unwrap();
        assert_eq!(
            s.poll_event().unwrap(),
            Some(Event::Packet {
                src: peer,
                data: b"via buffer".to_vec()
            })
        );
        assert_eq!(s.poll_event().unwrap(), None);
    }

    #[test]
    fn wanted_completes_one_frame() {
        let (mut s, _) = handshake(false);
        let f = frame(FRAME_RECV_PACKET, &[1u8; 100]);
        assert_eq!(s.wanted(4096), 5);
        s.feed(&f[..5]).unwrap();
        assert_eq!(s.wanted(4096), 100);
        assert_eq!(s.wanted(64), 64);
        s.feed(&f[5..50]).unwrap();
        assert_eq!(s.wanted(4096), 55);
    }

    #[test]
    fn bad_servers_are_errors() {
        let key = KeyPair::generate();
        let mut s = DerpSession::new("h", &key);
        assert_eq!(
            s.feed(b"HTTP/1.1 403 Forbidden\r\n\r\n"),
            Err(Error::UpgradeFailed(403))
        );
        let mut s = DerpSession::new("h", &key);
        assert_eq!(
            s.feed(&frame(FRAME_SERVER_INFO, &[0; 40])),
            Err(Error::BadGreeting)
        );
        let mut s = DerpSession::new("h", &key);
        assert_eq!(
            s.feed(&[FRAME_SERVER_KEY, 0xff, 0xff, 0xff, 0xff]),
            Err(Error::FrameTooLarge)
        );
        // A server info sealed for someone else.
        let (mut s, _) = {
            let server = KeyPair::generate();
            let mut s = DerpSession::new("h", &key);
            let mut kp = MAGIC.to_vec();
            kp.extend_from_slice(server.public());
            s.feed(&frame(FRAME_SERVER_KEY, &kp)).unwrap();
            (s, server)
        };
        let other = KeyPair::generate();
        let mut si = [1u8; 24].to_vec();
        si.extend(nacl_box_seal(
            other.secret(),
            key.public(),
            &[1u8; 24],
            b"{}",
        ));
        assert_eq!(
            s.feed(&frame(FRAME_SERVER_INFO, &si)),
            Err(Error::ServerInfoAuth)
        );
    }
}
