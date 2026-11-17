//! Tailscale's ts2021 control transport: a Noise IK handshake carried in an
//! HTTP/1.1 `Upgrade: tailscale-control-protocol` request, followed by
//! length-prefixed encrypted records over which HTTP/2 is spoken.
//!
//! [`ClientUpgrade`] (the upgrade and the handshake) and [`NoiseTransport`]
//! (the records) are the protocol without I/O. [`dial`] and [`NoiseConn`]
//! run them over a blocking [`Conn`].

use crate::crypto::{aead_encrypt, x25519_generate, x25519_public, TAG_LEN};
use crate::http1;
use crate::net::Conn;
use crate::noise::SymmetricState;
use alloc::string::String;
use alloc::vec::Vec;
use base64::Engine as _;
use std::io::{self, Read, Write};
use std::time::Duration;

pub const PROTOCOL_NAME: &[u8] = b"Noise_IK_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE_PREFIX: &str = "Tailscale Control Protocol v";
pub const UPGRADE_VALUE: &str = "tailscale-control-protocol";
pub const HANDSHAKE_HEADER: &str = "X-Tailscale-Handshake";
pub const UPGRADE_PATH: &str = "/ts2021";

const MSG_INITIATION: u8 = 1;
const MSG_RESPONSE: u8 = 2;
const MSG_ERROR: u8 = 3;
const MSG_RECORD: u8 = 4;
const HEADER_LEN: usize = 3;
/// Initiation frames carry a 2-byte protocol version before the usual header.
const INITIATION_HEADER_LEN: usize = 5;
const INITIATION_LEN: usize = 101;
const RESPONSE_LEN: usize = 51;
/// Maximum frame size on the wire including the 3-byte header.
const MAX_MESSAGE: usize = 4096;
const MAX_CIPHERTEXT: usize = MAX_MESSAGE - HEADER_LEN;
pub const MAX_PLAINTEXT: usize = MAX_CIPHERTEXT - TAG_LEN;

/// The control transport counts record nonces big-endian (unlike WireGuard).
fn be_nonce_key(counter: u64) -> u64 {
    // aead_* take a little-endian counter; feed the byte-swapped value so the
    // resulting 8 nonce bytes are big-endian.
    counter.swap_bytes()
}

fn aead_encrypt_be(key: &[u8; 32], counter: u64, plaintext: &[u8]) -> Vec<u8> {
    aead_encrypt(key, be_nonce_key(counter), plaintext, &[])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    BadHead(http1::HeadError),
    /// The upgrade was answered with another status (and whatever body came).
    UpgradeFailed(u16, String),
    UpgradeMismatch,
    /// The server refused the handshake, with its reason.
    Rejected(String),
    BadResponse(u8),
    BadResponseLength,
    ResponseAuth,
    BadInitiation,
    /// An error message from the server on an established transport.
    Server(String),
    UnexpectedType(u8),
    ShortRecord,
    RecordAuth,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::BadHead(e) => write!(f, "ts2021: {e}"),
            Error::UpgradeFailed(s, body) => write!(f, "ts2021 upgrade failed: status {s} {body}"),
            Error::UpgradeMismatch => write!(f, "ts2021 upgrade header mismatch"),
            Error::Rejected(m) => write!(f, "control server rejected handshake: {m}"),
            Error::BadResponse(t) => write!(f, "bad noise response (type {t})"),
            Error::BadResponseLength => write!(f, "bad noise response length"),
            Error::ResponseAuth => write!(f, "noise response authentication failed"),
            Error::BadInitiation => write!(f, "bad noise initiation"),
            Error::Server(m) => write!(f, "control server error: {m}"),
            Error::UnexpectedType(t) => write!(f, "unexpected noise message type {t}"),
            Error::ShortRecord => write!(f, "short noise record"),
            Error::RecordAuth => write!(f, "noise record authentication failed"),
        }
    }
}

impl core::error::Error for Error {}

fn io_error(e: Error) -> io::Error {
    let kind = match e {
        Error::UpgradeFailed(..) => io::ErrorKind::ConnectionRefused,
        Error::Rejected(_) | Error::ResponseAuth => io::ErrorKind::PermissionDenied,
        Error::Server(_) => io::ErrorKind::Other,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, e)
}

fn prologue(version: u16) -> Vec<u8> {
    alloc::format!("{PROLOGUE_PREFIX}{version}").into_bytes()
}

fn set_header(buf: &mut [u8], msg_type: u8, len: usize) {
    buf[0] = msg_type;
    buf[1..3].copy_from_slice(&(len as u16).to_be_bytes());
}

/// Client side of the handshake, split so the initiation can be sent inside
/// an HTTP header before the response bytes arrive.
pub struct ClientHandshake {
    state: SymmetricState,
    ephemeral_secret: [u8; 32],
    machine_secret: [u8; 32],
    pub initiation: [u8; INITIATION_LEN],
}

impl ClientHandshake {
    pub fn new(machine_secret: &[u8; 32], control_pub: &[u8; 32], version: u16) -> Self {
        let mut s = SymmetricState::new(PROTOCOL_NAME);
        s.mix_hash(&prologue(version));
        // <- s (pre-message: the control server's static key)
        s.mix_hash(control_pub);

        // Layout: version(2) | type(1) | payload length(2) | payload(96).
        let mut init = [0u8; INITIATION_LEN];
        init[0..2].copy_from_slice(&version.to_be_bytes());
        set_header(
            &mut init[2..5],
            MSG_INITIATION,
            INITIATION_LEN - INITIATION_HEADER_LEN,
        );

        // -> e, es, s, ss
        let ephemeral_secret = x25519_generate();
        let ephemeral_pub = x25519_public(&ephemeral_secret);
        init[5..37].copy_from_slice(&ephemeral_pub);
        s.mix_hash(&ephemeral_pub);
        s.mix_dh(&ephemeral_secret, control_pub);
        let machine_pub = x25519_public(machine_secret);
        let enc_static = s.encrypt_and_hash(&machine_pub);
        init[37..85].copy_from_slice(&enc_static);
        s.mix_dh(machine_secret, control_pub);
        let tag = s.encrypt_and_hash(&[]);
        init[85..101].copy_from_slice(&tag);

        ClientHandshake {
            state: s,
            ephemeral_secret,
            machine_secret: *machine_secret,
            initiation: init,
        }
    }

    pub fn initiation_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.initiation)
    }

    /// Consumes the 51-byte response and derives the transport keys.
    pub fn finish(mut self, response: &[u8]) -> Result<SessionKeys, Error> {
        if response.len() != RESPONSE_LEN || response[0] != MSG_RESPONSE {
            return Err(Error::BadResponse(response.first().copied().unwrap_or(0)));
        }
        let len = u16::from_be_bytes([response[1], response[2]]) as usize;
        if len != RESPONSE_LEN - HEADER_LEN {
            return Err(Error::BadResponseLength);
        }
        // <- e, ee, se
        let re: [u8; 32] = response[3..35].try_into().unwrap();
        self.state.mix_hash(&re);
        self.state.mix_dh(&self.ephemeral_secret, &re);
        self.state.mix_dh(&self.machine_secret, &re);
        self.state
            .decrypt_and_hash(&response[35..51])
            .map_err(|_| Error::ResponseAuth)?;
        let (tx, rx) = self.state.split();
        Ok(SessionKeys { tx, rx })
    }
}

pub struct SessionKeys {
    pub tx: [u8; 32],
    pub rx: [u8; 32],
}

/// Server side of the handshake. Only used by tests and by tools; the device is
/// always the client.
pub fn server_handshake(
    control_secret: &[u8; 32],
    initiation: &[u8],
) -> Result<(SessionKeys, [u8; 32], Vec<u8>), Error> {
    if initiation.len() != INITIATION_LEN || initiation[2] != MSG_INITIATION {
        return Err(Error::BadInitiation);
    }
    let version = u16::from_be_bytes([initiation[0], initiation[1]]);
    let control_pub = x25519_public(control_secret);
    let mut s = SymmetricState::new(PROTOCOL_NAME);
    s.mix_hash(&prologue(version));
    s.mix_hash(&control_pub);
    let re: [u8; 32] = initiation[5..37].try_into().unwrap();
    s.mix_hash(&re);
    s.mix_dh(control_secret, &re);
    let machine_pub: [u8; 32] = s
        .decrypt_and_hash(&initiation[37..85])
        .map_err(|_| Error::BadInitiation)?
        .try_into()
        .unwrap();
    s.mix_dh(control_secret, &machine_pub);
    s.decrypt_and_hash(&initiation[85..101])
        .map_err(|_| Error::BadInitiation)?;

    let mut resp = [0u8; RESPONSE_LEN];
    set_header(&mut resp, MSG_RESPONSE, RESPONSE_LEN - HEADER_LEN);
    let e = x25519_generate();
    let ep = x25519_public(&e);
    resp[3..35].copy_from_slice(&ep);
    s.mix_hash(&ep);
    s.mix_dh(&e, &re);
    s.mix_dh(&e, &machine_pub);
    let tag = s.encrypt_and_hash(&[]);
    resp[35..51].copy_from_slice(&tag);
    let (k_client_to_server, k_server_to_client) = s.split();
    Ok((
        SessionKeys {
            tx: k_server_to_client,
            rx: k_client_to_server,
        },
        machine_pub,
        resp.to_vec(),
    ))
}

/// The records of an established transport, without I/O.
///
/// Received records are decrypted one at a time where they lie in the
/// receive buffer, so the transport holds at most about one record (4 KB):
/// a second buffer for the plaintext did not fit an ESP32-C3 while the
/// network map arrived.
pub struct NoiseTransport {
    tx_key: [u8; 32],
    tx_n: u64,
    rx_key: [u8; 32],
    rx_n: u64,
    /// Received bytes, starting with the record being read.
    raw: Vec<u8>,
    /// The decrypted part of the first record not read yet: `raw[start..end]`.
    plain: Option<(usize, usize)>,
    /// Length of `raw` before the space handed out by `read_buffer`.
    recv_start: usize,
}

impl NoiseTransport {
    pub fn new(keys: SessionKeys) -> Self {
        NoiseTransport {
            tx_key: keys.tx,
            tx_n: 0,
            rx_key: keys.rx,
            rx_n: 0,
            raw: Vec::new(),
            plain: None,
            recv_start: 0,
        }
    }

    /// Appends `plain` to `out` as records.
    pub fn seal(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        for chunk in plain.chunks(MAX_PLAINTEXT) {
            let ct = aead_encrypt_be(&self.tx_key, self.tx_n, chunk);
            self.tx_n += 1;
            let mut hdr = [0u8; HEADER_LEN];
            set_header(&mut hdr, MSG_RECORD, ct.len());
            out.extend_from_slice(&hdr);
            out.extend_from_slice(&ct);
        }
    }

    /// Takes received bytes.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        self.raw.extend_from_slice(data);
        self.advance()
    }

    /// How many more bytes complete the next record (its header first):
    /// what a caller reading from a connection should ask for. 0 while
    /// decrypted bytes wait to be read.
    pub fn wanted(&self) -> usize {
        if self.plain.is_some() {
            return 0;
        }
        if self.raw.len() < HEADER_LEN {
            return HEADER_LEN - self.raw.len();
        }
        let len = u16::from_be_bytes([self.raw[1], self.raw[2]]) as usize;
        (HEADER_LEN + len).saturating_sub(self.raw.len())
    }

    /// Space for reading up to `size` received bytes straight into the
    /// transport; then [`received`](Self::received).
    pub fn read_buffer(&mut self, size: usize) -> &mut [u8] {
        self.recv_start = self.raw.len();
        self.raw.resize(self.recv_start + size, 0);
        &mut self.raw[self.recv_start..]
    }

    /// `n` bytes were read into the [`read_buffer`](Self::read_buffer);
    /// like [`feed`](Self::feed) with them.
    pub fn received(&mut self, n: usize) -> Result<(), Error> {
        self.raw.truncate(self.recv_start + n);
        self.advance()
    }

    /// Decrypts the first record if it is complete and nothing of the
    /// previous one waits to be read.
    fn advance(&mut self) -> Result<(), Error> {
        if self.plain.is_some() || self.raw.len() < HEADER_LEN {
            return Ok(());
        }
        let len = u16::from_be_bytes([self.raw[1], self.raw[2]]) as usize;
        match self.raw[0] {
            MSG_ERROR => {
                if self.raw.len() >= HEADER_LEN + len {
                    let msg = &self.raw[HEADER_LEN..HEADER_LEN + len];
                    return Err(Error::Server(String::from_utf8_lossy(msg).into_owned()));
                }
                return Ok(());
            }
            MSG_RECORD => {}
            t => return Err(Error::UnexpectedType(t)),
        }
        if !(TAG_LEN..=MAX_CIPHERTEXT).contains(&len) {
            return Err(Error::ShortRecord);
        }
        if self.raw.len() < HEADER_LEN + len {
            return Ok(());
        }
        let n = crate::crypto::aead_decrypt_slice(
            &self.rx_key,
            be_nonce_key(self.rx_n),
            &mut self.raw[HEADER_LEN..HEADER_LEN + len],
            &[],
        )
        .map_err(|_| Error::RecordAuth)?;
        self.rx_n += 1;
        self.plain = Some((HEADER_LEN, HEADER_LEN + n));
        if n == 0 {
            self.consume(0);
        }
        Ok(())
    }

    /// Decrypted bytes not read yet (from one record at a time).
    pub fn plaintext(&self) -> &[u8] {
        match self.plain {
            Some((start, end)) => &self.raw[start..end],
            None => &[],
        }
    }

    /// Marks the first `n` bytes of [`plaintext`](Self::plaintext) as read.
    /// Once a record is read through, the next one is decrypted.
    pub fn consume(&mut self, n: usize) {
        let Some((start, end)) = self.plain else {
            return;
        };
        let start = (start + n).min(end);
        if start < end {
            self.plain = Some((start, end));
            return;
        }
        // The record was read through: drop it with its tag.
        self.plain = None;
        self.raw.drain(..end + TAG_LEN);
        // A following record already received is decrypted now; its
        // errors are reported by the next feed or received.
        let _ = self.advance();
    }
}

/// The client side of the HTTP upgrade and the Noise handshake, without
/// I/O: the request is the first output; [`feed`](Self::feed) returns the
/// transport keys and the bytes after the handshake once they are in.
pub struct ClientUpgrade {
    hs: Option<ClientHandshake>,
    inbuf: Vec<u8>,
    upgraded: bool,
    out: Vec<u8>,
}

impl ClientUpgrade {
    pub fn new(
        host: &str,
        machine_secret: &[u8; 32],
        control_pub: &[u8; 32],
        version: u16,
    ) -> Self {
        let hs = ClientHandshake::new(machine_secret, control_pub, version);
        let out = http1::request(
            "POST",
            UPGRADE_PATH,
            host,
            &[
                ("Upgrade", UPGRADE_VALUE),
                ("Connection", "upgrade"),
                (HANDSHAKE_HEADER, &hs.initiation_base64()),
            ],
        );
        ClientUpgrade {
            hs: Some(hs),
            inbuf: Vec::new(),
            upgraded: false,
            out,
        }
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    /// Takes bytes from the server; `Some` once the handshake is complete,
    /// with the bytes that followed it.
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<(SessionKeys, Vec<u8>)>, Error> {
        self.inbuf.extend_from_slice(data);
        if !self.upgraded {
            let Some(head) = http1::parse_response_head(&self.inbuf).map_err(Error::BadHead)?
            else {
                return Ok(None);
            };
            if head.status != 101 {
                let body = &head.leftover[..head.leftover.len().min(300)];
                return Err(Error::UpgradeFailed(
                    head.status,
                    String::from_utf8_lossy(body).into_owned(),
                ));
            }
            if !head
                .header("upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case(UPGRADE_VALUE))
            {
                return Err(Error::UpgradeMismatch);
            }
            self.upgraded = true;
            self.inbuf = head.leftover;
        }
        if self.inbuf.len() < HEADER_LEN {
            return Ok(None);
        }
        // An unauthenticated error message instead of the response.
        if self.inbuf[0] == MSG_ERROR {
            let len = u16::from_be_bytes([self.inbuf[1], self.inbuf[2]]) as usize;
            if self.inbuf.len() < HEADER_LEN + len {
                return Ok(None);
            }
            let msg = &self.inbuf[HEADER_LEN..HEADER_LEN + len];
            return Err(Error::Rejected(String::from_utf8_lossy(msg).into_owned()));
        }
        if self.inbuf.len() < RESPONSE_LEN {
            return Ok(None);
        }
        let keys = self
            .hs
            .take()
            .ok_or(Error::BadResponseLength)?
            .finish(&self.inbuf[..RESPONSE_LEN])?;
        Ok(Some((keys, self.inbuf.split_off(RESPONSE_LEN))))
    }
}

/// A [`NoiseTransport`] over a blocking connection.
pub struct NoiseConn {
    inner: Box<dyn Conn>,
    transport: NoiseTransport,
}

impl NoiseConn {
    pub fn new(inner: Box<dyn Conn>, keys: SessionKeys, leftover: Vec<u8>) -> io::Result<Self> {
        let mut transport = NoiseTransport::new(keys);
        transport.feed(&leftover).map_err(io_error)?;
        Ok(NoiseConn { inner, transport })
    }
}

impl Read for NoiseConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.transport.plaintext().is_empty() {
            let want = self.transport.wanted();
            let result = self.inner.read(self.transport.read_buffer(want));
            let n = result.inspect_err(|_| {
                let _ = self.transport.received(0);
            })?;
            self.transport.received(n).map_err(io_error)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "noise transport closed",
                ));
            }
        }
        let avail = self.transport.plaintext();
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.transport.consume(n);
        Ok(n)
    }
}

impl Write for NoiseConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = Vec::with_capacity(buf.len() + 64);
        self.transport.seal(buf, &mut out);
        self.inner.write_all(&out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Conn for NoiseConn {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }
}

/// Runs the full client handshake over `conn` using the HTTP upgrade.
pub fn dial(
    mut conn: Box<dyn Conn>,
    host: &str,
    machine_secret: &[u8; 32],
    control_pub: &[u8; 32],
    version: u16,
) -> io::Result<NoiseConn> {
    let mut up = ClientUpgrade::new(host, machine_secret, control_pub, version);
    conn.write_all(&up.take_output())?;
    conn.flush()?;
    conn.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut tmp = [0u8; 1024];
    loop {
        let n = conn.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed during noise handshake",
            ));
        }
        if let Some((keys, leftover)) = up.feed(&tmp[..n]).map_err(io_error)? {
            return NoiseConn::new(conn, keys, leftover);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::x25519_public;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// The server's half: answers the upgrade request in `req`.
    fn serve_upgrade(control_secret: &[u8; 32], req: &[u8]) -> (SessionKeys, Vec<u8>) {
        let req = std::str::from_utf8(req).unwrap();
        let b64 = req
            .lines()
            .find_map(|l| l.strip_prefix("X-Tailscale-Handshake: "))
            .unwrap();
        let init = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let (keys, _, resp) = server_handshake(control_secret, &init).unwrap();
        let mut reply =
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: tailscale-control-protocol\r\n\r\n"
                .to_vec();
        reply.extend(resp);
        (keys, reply)
    }

    #[test]
    fn upgrade_and_records_without_io() {
        let control_secret = x25519_generate();
        let control_pub = x25519_public(&control_secret);
        let mut up = ClientUpgrade::new("ctl.example", &x25519_generate(), &control_pub, 106);
        let req = up.take_output();
        assert!(req.starts_with(b"POST /ts2021 HTTP/1.1\r\nHost: ctl.example\r\n"));
        let (server_keys, mut reply) = serve_upgrade(&control_secret, &req);
        let mut server = NoiseTransport::new(server_keys);
        // A record right behind the handshake, all fed a byte at a time.
        server.seal(b"early", &mut reply);
        let mut done = None;
        let mut fed = 0;
        while done.is_none() {
            done = up.feed(&reply[fed..fed + 1]).unwrap();
            fed += 1;
        }
        let (keys, mut leftover) = done.unwrap();
        // Done right after the 51-byte response, before the record.
        leftover.extend_from_slice(&reply[fed..]);
        let mut client = NoiseTransport::new(keys);
        client.feed(&leftover).unwrap();
        assert_eq!(client.plaintext(), b"early");
        client.consume(5);

        // Both directions, and a message larger than one record.
        let big = vec![3u8; MAX_PLAINTEXT * 2 + 10];
        let mut wire = Vec::new();
        client.seal(&big, &mut wire);
        server.feed(&wire).unwrap();
        // One record at a time.
        let mut got = Vec::new();
        while !server.plaintext().is_empty() {
            assert!(server.plaintext().len() <= MAX_PLAINTEXT);
            got.extend_from_slice(server.plaintext());
            let n = server.plaintext().len();
            server.consume(n);
        }
        assert_eq!(got, big);
        let mut wire = Vec::new();
        server.seal(b"back", &mut wire);
        let (a, b) = wire.split_at(7);
        client.feed(a).unwrap();
        assert!(client.plaintext().is_empty());
        client.feed(b).unwrap();
        assert_eq!(client.plaintext(), b"back");

        // Read straight into the transport's buffer, asking for what is
        // wanted: the header, then the rest of the record.
        let mut wire = Vec::new();
        server.seal(b"buffered", &mut wire);
        client.consume(2);
        assert_eq!(client.plaintext(), b"ck");
        assert_eq!(client.wanted(), 0);
        client.consume(2);
        let mut rest = &wire[..];
        while client.plaintext().is_empty() {
            let want = client.wanted();
            assert!(want > 0 && want <= rest.len());
            client.read_buffer(want).copy_from_slice(&rest[..want]);
            client.received(want).unwrap();
            rest = &rest[want..];
        }
        assert!(rest.is_empty());
        assert_eq!(client.plaintext(), b"buffered");
        client.consume(8);

        // A tampered record.
        let mut wire = Vec::new();
        server.seal(b"x", &mut wire);
        let last = wire.len() - 1;
        wire[last] ^= 1;
        assert_eq!(client.feed(&wire), Err(Error::RecordAuth));
    }

    #[test]
    fn refused_upgrades() {
        let control_pub = x25519_public(&x25519_generate());
        let mut up = ClientUpgrade::new("h", &x25519_generate(), &control_pub, 106);
        assert_eq!(
            up.feed(b"HTTP/1.1 403 Forbidden\r\n\r\nno").err(),
            Some(Error::UpgradeFailed(403, "no".into()))
        );
        let mut up = ClientUpgrade::new("h", &x25519_generate(), &control_pub, 106);
        assert_eq!(
            up.feed(b"HTTP/1.1 101 OK\r\nUpgrade: websocket\r\n\r\n")
                .err(),
            Some(Error::UpgradeMismatch)
        );
        let mut up = ClientUpgrade::new("h", &x25519_generate(), &control_pub, 106);
        let mut reply = b"HTTP/1.1 101 OK\r\nUpgrade: tailscale-control-protocol\r\n\r\n".to_vec();
        reply.extend_from_slice(&[MSG_ERROR, 0, 4]);
        reply.extend_from_slice(b"busy");
        assert_eq!(up.feed(&reply).err(), Some(Error::Rejected("busy".into())));
        // A response for another client's handshake.
        let mut up = ClientUpgrade::new("h", &x25519_generate(), &control_pub, 106);
        let other = x25519_generate();
        let mut other_up = ClientUpgrade::new("h", &x25519_generate(), &x25519_public(&other), 106);
        let (_, reply) = serve_upgrade(&other, &other_up.take_output());
        assert_eq!(up.feed(&reply).err(), Some(Error::ResponseAuth));
    }

    #[test]
    fn handshake_and_records_roundtrip() {
        let control_secret = x25519_generate();
        let control_pub = x25519_public(&control_secret);
        let machine_secret = x25519_generate();
        let machine_pub = x25519_public(&machine_secret);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut init = [0u8; INITIATION_LEN];
            s.read_exact(&mut init).unwrap();
            let (keys, mpub, resp) = server_handshake(&control_secret, &init).unwrap();
            assert_eq!(mpub, machine_pub);
            s.write_all(&resp).unwrap();
            let mut nc = NoiseConn::new(Box::new(s), keys, vec![]).unwrap();
            let mut buf = [0u8; 5];
            nc.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hello");
            nc.write_all(b"world").unwrap();
            let big = vec![7u8; 100_000];
            nc.write_all(&big).unwrap();
        });

        let mut c = TcpStream::connect(addr).unwrap();
        let hs = ClientHandshake::new(&machine_secret, &control_pub, 106);
        c.write_all(&hs.initiation).unwrap();
        let mut resp = [0u8; RESPONSE_LEN];
        c.read_exact(&mut resp).unwrap();
        let keys = hs.finish(&resp).unwrap();
        let mut nc = NoiseConn::new(Box::new(c), keys, vec![]).unwrap();
        nc.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        nc.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"world");
        let mut big = vec![0u8; 100_000];
        nc.read_exact(&mut big).unwrap();
        assert!(big.iter().all(|&b| b == 7));
        server.join().unwrap();
    }
}
