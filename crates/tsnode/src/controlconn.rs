//! One connection to the control server, without I/O: the HTTP upgrade to
//! ts2021 and the Noise handshake, the early payload the server may send,
//! then HTTP/2 inside the Noise records.
//!
//! The caller moves bytes: what [`ControlConn::take_output`] returns goes to
//! the server; what the server sends comes in through
//! [`ControlConn::read_buffer`] and [`ControlConn::received`] (or
//! [`ControlConn::feed`]). Requests can be made once
//! [`ControlConn::is_ready`].

use crate::controlbase::{self, ClientUpgrade, NoiseTransport};
use crate::h2::{self, Body, H2Session};
use alloc::string::String;
use alloc::vec::Vec;
use log::debug;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Noise(controlbase::Error),
    H2(h2::Error),
    EarlyPayloadTooLarge,
    /// The handshake is not finished yet, or failed.
    NotReady,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Noise(e) => write!(f, "{e}"),
            Error::H2(e) => write!(f, "{e}"),
            Error::EarlyPayloadTooLarge => write!(f, "early payload too large"),
            Error::NotReady => write!(f, "control connection not ready"),
        }
    }
}

impl core::error::Error for Error {}

impl From<controlbase::Error> for Error {
    fn from(e: controlbase::Error) -> Self {
        Error::Noise(e)
    }
}

impl From<h2::Error> for Error {
    fn from(e: h2::Error) -> Self {
        Error::H2(e)
    }
}

/// What a fresh ts2021 transport starts with.
#[derive(Debug, PartialEq, Eq)]
pub enum Early {
    /// Too few bytes to tell.
    NeedMore,
    /// An early payload (a node-key challenge) of `len` bytes in all.
    Payload { json: Vec<u8>, len: usize },
    /// HTTP/2 right away.
    None,
}

const EARLY_MAGIC: &[u8; 5] = b"\xff\xff\xffTS";
const MAX_EARLY_PAYLOAD: usize = 1 << 20;

/// Looks for the early payload the server may send before HTTP/2:
/// magic `\xff\xff\xffTS`, u32 BE length, JSON.
pub fn early_payload(buf: &[u8]) -> Result<Early, Error> {
    let n = buf.len().min(EARLY_MAGIC.len());
    if buf[..n] != EARLY_MAGIC[..n] {
        return Ok(Early::None);
    }
    if buf.len() < 9 {
        return Ok(Early::NeedMore);
    }
    let len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
    if len > MAX_EARLY_PAYLOAD {
        return Err(Error::EarlyPayloadTooLarge);
    }
    if buf.len() < 9 + len {
        return Ok(Early::NeedMore);
    }
    Ok(Early::Payload {
        json: buf[9..9 + len].to_vec(),
        len: 9 + len,
    })
}

/// Bytes read at a time during the upgrade, before records have lengths.
const UPGRADE_READ: usize = 1024;

enum Stage {
    Upgrade(ClientUpgrade),
    /// Handshake done; reading what comes before HTTP/2.
    Early {
        noise: NoiseTransport,
        buf: Vec<u8>,
    },
    Ready {
        noise: NoiseTransport,
        h2: H2Session,
    },
    Failed,
}

pub struct ControlConn {
    stage: Stage,
    authority: String,
    out: Vec<u8>,
    /// Receive space while the upgrade runs.
    scratch: Vec<u8>,
}

impl ControlConn {
    /// Starts the upgrade to the server `authority` (its Host header) with
    /// the machine key `machine_secret`; `control_pub` is the server's key
    /// from `/key`. The upgrade request is the first output.
    pub fn new(
        authority: &str,
        machine_secret: &[u8; 32],
        control_pub: &[u8; 32],
        version: u16,
    ) -> Self {
        let mut up = ClientUpgrade::new(authority, machine_secret, control_pub, version);
        let out = up.take_output();
        ControlConn {
            stage: Stage::Upgrade(up),
            authority: authority.into(),
            out,
            scratch: Vec::new(),
        }
    }

    /// True once HTTP/2 requests can be made.
    pub fn is_ready(&self) -> bool {
        matches!(self.stage, Stage::Ready { .. })
    }

    /// Bytes to send to the server.
    pub fn take_output(&mut self) -> Vec<u8> {
        self.collect_output();
        core::mem::take(&mut self.out)
    }

    pub fn has_output(&mut self) -> bool {
        self.collect_output();
        !self.out.is_empty()
    }

    fn collect_output(&mut self) {
        if let Stage::Ready { noise, h2 } = &mut self.stage {
            if h2.has_output() {
                noise.seal(&h2.take_output(), &mut self.out);
            }
        }
    }

    /// How many bytes to read next, at most `max`: what completes the next
    /// record, so that a read never holds more than one.
    pub fn wanted(&self, max: usize) -> usize {
        let w = match &self.stage {
            Stage::Upgrade(_) | Stage::Failed => UPGRADE_READ,
            Stage::Early { noise, .. } | Stage::Ready { noise, .. } => noise.wanted(),
        };
        w.clamp(1, max.max(1))
    }

    /// Space to read up to `size` received bytes into; then
    /// [`received`](Self::received).
    pub fn read_buffer(&mut self, size: usize) -> &mut [u8] {
        match &mut self.stage {
            Stage::Early { noise, .. } | Stage::Ready { noise, .. } => noise.read_buffer(size),
            _ => {
                self.scratch.resize(size, 0);
                &mut self.scratch
            }
        }
    }

    /// `n` bytes were read into the [`read_buffer`](Self::read_buffer).
    pub fn received(&mut self, n: usize) -> Result<(), Error> {
        let r = match &mut self.stage {
            Stage::Early { noise, .. } | Stage::Ready { noise, .. } => {
                noise.received(n).map_err(Error::from)
            }
            Stage::Upgrade(_) => {
                let data = core::mem::take(&mut self.scratch);
                let r = self.upgrade_input(&data[..n.min(data.len())]);
                self.scratch = data;
                r
            }
            Stage::Failed => Err(Error::NotReady),
        };
        let r = r.and_then(|()| self.advance());
        if r.is_err() {
            self.stage = Stage::Failed;
        }
        r
    }

    /// Takes bytes from the server.
    pub fn feed(&mut self, mut data: &[u8]) -> Result<(), Error> {
        while !data.is_empty() {
            let n = data.len().min(self.wanted(data.len()));
            self.read_buffer(n).copy_from_slice(&data[..n]);
            self.received(n)?;
            data = &data[n..];
        }
        Ok(())
    }

    fn upgrade_input(&mut self, data: &[u8]) -> Result<(), Error> {
        let Stage::Upgrade(up) = &mut self.stage else {
            return Ok(());
        };
        if let Some((keys, leftover)) = up.feed(data)? {
            debug!("control: noise handshake complete");
            self.scratch = Vec::new();
            let mut noise = NoiseTransport::new(keys);
            noise.feed(&leftover)?;
            self.stage = Stage::Early {
                noise,
                buf: Vec::new(),
            };
        }
        Ok(())
    }

    /// Hands decrypted bytes on: to the early-payload check, then to HTTP/2.
    fn advance(&mut self) -> Result<(), Error> {
        loop {
            match &mut self.stage {
                Stage::Early { noise, buf } => {
                    let p = noise.plaintext();
                    if p.is_empty() {
                        return Ok(());
                    }
                    buf.extend_from_slice(p);
                    let n = p.len();
                    noise.consume(n);
                    let prefix = match early_payload(buf)? {
                        Early::NeedMore => continue,
                        Early::Payload { json, len } => {
                            debug!("control: early payload {}", String::from_utf8_lossy(&json));
                            buf.split_off(len)
                        }
                        Early::None => core::mem::take(buf),
                    };
                    let Stage::Early { noise, .. } =
                        core::mem::replace(&mut self.stage, Stage::Failed)
                    else {
                        unreachable!()
                    };
                    let mut h2 = H2Session::new(&self.authority, prefix);
                    // Frames that came with the prefix.
                    h2.feed(&[])?;
                    self.stage = Stage::Ready { noise, h2 };
                }
                Stage::Ready { noise, h2 } => {
                    let p = noise.plaintext();
                    if p.is_empty() {
                        return Ok(());
                    }
                    h2.feed(p)?;
                    let n = p.len();
                    noise.consume(n);
                }
                _ => return Ok(()),
            }
        }
    }

    fn h2(&mut self) -> Result<&mut H2Session, Error> {
        match &mut self.stage {
            Stage::Ready { h2, .. } => Ok(h2),
            _ => Err(Error::NotReady),
        }
    }

    /// Queues a request; see [`H2Session::request`].
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<u32, Error> {
        Ok(self.h2()?.request(method, path, headers, body)?)
    }

    pub fn body_unsent(&mut self, id: u32) -> bool {
        self.h2().is_ok_and(|h| h.body_unsent(id))
    }

    pub fn status(&mut self, id: u32) -> Result<Option<u16>, Error> {
        Ok(self.h2()?.status(id)?)
    }

    pub fn take_body(&mut self, id: u32) -> Result<Body, Error> {
        Ok(self.h2()?.take_body(id)?)
    }

    pub fn forget_stream(&mut self, id: u32) {
        if let Ok(h) = self.h2() {
            h.forget_stream(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controlbase::server_handshake;
    use crate::crypto::{x25519_generate, x25519_public};
    use base64::Engine;

    fn frame(typ: u8, flags: u8, sid: u32, payload: &[u8]) -> Vec<u8> {
        let mut f = (payload.len() as u32).to_be_bytes()[1..].to_vec();
        f.extend_from_slice(&[typ, flags]);
        f.extend_from_slice(&sid.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn early_payload_or_http2() {
        assert_eq!(early_payload(b"").unwrap(), Early::NeedMore);
        assert_eq!(early_payload(b"\xff\xff").unwrap(), Early::NeedMore);
        assert_eq!(early_payload(b"PRI * HTTP/2.0").unwrap(), Early::None);
        let mut buf = b"\xff\xff\xffTS\x00\x00\x00\x02{}".to_vec();
        assert_eq!(early_payload(&buf[..10]).unwrap(), Early::NeedMore);
        buf.extend_from_slice(b"PRI");
        assert_eq!(
            early_payload(&buf).unwrap(),
            Early::Payload {
                json: b"{}".to_vec(),
                len: 11
            }
        );
        assert!(early_payload(b"\xff\xff\xffTS\x7f\x00\x00\x00").is_err());
    }

    /// The whole connection in memory: upgrade, an early payload, a request
    /// and its response, with the server's bytes arriving a few at a time.
    #[test]
    fn a_request_over_a_fresh_connection() {
        let control_secret = x25519_generate();
        let mut c = ControlConn::new(
            "ctl.example",
            &x25519_generate(),
            &x25519_public(&control_secret),
            106,
        );
        assert!(!c.is_ready());
        let req = String::from_utf8(c.take_output()).unwrap();
        let b64 = req
            .lines()
            .find_map(|l| l.strip_prefix("X-Tailscale-Handshake: "))
            .unwrap();
        let init = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let (keys, _, resp) = server_handshake(&control_secret, &init).unwrap();
        let mut server = NoiseTransport::new(keys);
        let mut wire =
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: tailscale-control-protocol\r\n\r\n"
                .to_vec();
        wire.extend(resp);
        let mut plain = b"\xff\xff\xffTS\x00\x00\x00\x02{}".to_vec();
        plain.extend(frame(4, 0, 0, &[])); // SETTINGS
        server.seal(&plain, &mut wire);
        for piece in wire.chunks(7) {
            c.feed(piece).unwrap();
        }
        assert!(c.is_ready());

        let id = c.request("POST", "/machine/map", &[], b"{}").unwrap();
        // Preface, settings and the request, sealed in records.
        server.feed(&c.take_output()).unwrap();
        let mut got = Vec::new();
        while !server.plaintext().is_empty() {
            got.extend_from_slice(server.plaintext());
            let n = server.plaintext().len();
            server.consume(n);
        }
        assert!(got.starts_with(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));

        let mut reply = frame(1, 4, id, &[0x88]); // HEADERS, :status 200
        reply.extend(frame(0, 1, id, b"body")); // DATA, END_STREAM
        let mut wire = Vec::new();
        server.seal(&reply, &mut wire);
        c.feed(&wire).unwrap();
        assert_eq!(c.status(id), Ok(Some(200)));
        assert_eq!(c.take_body(id), Ok(Body::Data(b"body".to_vec())));
        assert_eq!(c.take_body(id), Ok(Body::End));

        // A tampered record breaks the connection for good.
        let mut wire = Vec::new();
        server.seal(b"x", &mut wire);
        wire[5] ^= 1;
        assert!(c.feed(&wire).is_err());
        assert_eq!(c.request("GET", "/", &[], &[]), Err(Error::NotReady));
    }
}
