//! Talks to the control server over embassy-net sockets: fetches its key
//! from `/key` (HTTPS) and drives tsnode's sans-IO [`ControlConn`] (Noise
//! upgrade, then HTTP/2) over plain TCP port 80.

use crate::tls::TlsStream;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::{with_timeout, Duration};
use embedded_io_async::Write;
use serde::Deserialize;
use tsnode::controlconn::ControlConn;
use tsnode::h2::Body;
use tsnode::http1;
use tsnode::keys::{parse_prefixed, MACHINE_PUB_PREFIX};
use tsnode::types::CAPABILITY_VERSION;

pub const HOST: &str = "controlplane.tailscale.com";
const TIMEOUT: Duration = Duration::from_secs(30);

pub async fn resolve(stack: Stack<'static>, host: &str) -> Result<IpAddress, String> {
    let addrs = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|e| format!("dns {host}: {e:?}"))?;
    addrs
        .first()
        .copied()
        .ok_or_else(|| format!("dns {host}: no address"))
}

#[derive(Deserialize)]
struct KeyResponse {
    #[serde(rename = "publicKey", default)]
    public_key: String,
}

/// A byte stream an HTTP/1 response is read from.
#[allow(async_fn_in_trait)]
trait ReadSome {
    /// Reads at least one byte; 0 at the end of the stream.
    async fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, String>;
}

impl ReadSome for TcpSocket<'_> {
    async fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        match with_timeout(TIMEOUT, self.read(buf)).await {
            Ok(r) => r.map_err(|e| format!("read: {e:?}")),
            Err(_) => Err("read timeout".into()),
        }
    }
}

impl ReadSome for TlsStream<'_> {
    async fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        self.read(buf).await
    }
}

/// Reads a response head and (up to `max` bytes of) its body.
async fn read_response(
    r: &mut impl ReadSome,
    max: usize,
) -> Result<(http1::ResponseHead, Vec<u8>), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let head = loop {
        let n = r.read_some(&mut chunk).await?;
        if n == 0 {
            return Err("closed in the head".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(h) = http1::parse_response_head(&buf).map_err(|e| e.to_string())? {
            break h;
        }
    };
    let want = head
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok());
    let mut body = head.leftover.clone();
    while want.is_none_or(|w| body.len() < w) && body.len() < max {
        let n = r.read_some(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok((head, body))
}

fn key_request() -> Vec<u8> {
    let path = format!("/key?v={CAPABILITY_VERSION}");
    http1::request(
        "GET",
        &path,
        HOST,
        &[("Connection", "close"), ("Accept", "application/json")],
    )
}

/// Sets the clock from the `Date` of the control server's plain-HTTP answer
/// (a redirect to HTTPS). TLS needs the time to check certificates, and
/// this board has no clock that keeps it.
pub async fn sync_clock(stack: Stack<'static>, addr: IpAddress) -> Result<(), String> {
    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 512];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
    sock.set_timeout(Some(TIMEOUT));
    sock.connect((addr, 80))
        .await
        .map_err(|e| format!("clock: connect: {e:?}"))?;
    sock.write_all(&key_request())
        .await
        .map_err(|e| format!("clock: write: {e:?}"))?;
    let (head, _) = read_response(&mut sock, 0)
        .await
        .map_err(|e| format!("clock: {e}"))?;
    sock.close();
    let date = head.header("date").ok_or("clock: no Date header")?;
    if !crate::clock::set_from_http_date(date) {
        return Err(format!("clock: bad Date {date}"));
    }
    Ok(())
}

/// GET /key over HTTPS.
pub async fn fetch_key(stack: Stack<'static>, addr: IpAddress) -> Result<[u8; 32], String> {
    let mut rx = vec![0u8; 4096];
    let mut tx = vec![0u8; 2048];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
    sock.set_timeout(Some(TIMEOUT));
    sock.connect((addr, 443))
        .await
        .map_err(|e| format!("/key connect: {e:?}"))?;
    let mut tls = TlsStream::connect(sock, HOST).await?;
    tls.write_all(&key_request()).await?;
    let (head, body) = read_response(&mut tls, 4096)
        .await
        .map_err(|e| format!("/key: {e}"))?;
    tls.close();
    if head.status != 200 {
        return Err(format!("/key: HTTP {}", head.status));
    }
    let kr: KeyResponse =
        serde_json::from_slice(&body).map_err(|e| format!("/key: bad json: {e}"))?;
    parse_prefixed(MACHINE_PUB_PREFIX, &kr.public_key).ok_or_else(|| "/key: no publicKey".into())
}

/// A ts2021 connection: a [`ControlConn`] pumped over a TCP socket.
pub struct ControlIo<'a> {
    sock: TcpSocket<'a>,
    cc: ControlConn,
}

impl<'a> ControlIo<'a> {
    /// Connects to `addr`:80 and runs the upgrade and handshake.
    pub async fn open(
        stack: Stack<'static>,
        rx: &'a mut [u8],
        tx: &'a mut [u8],
        addr: IpAddress,
        machine_secret: &[u8; 32],
        control_pub: &[u8; 32],
    ) -> Result<Self, String> {
        let mut sock = TcpSocket::new(stack, rx, tx);
        sock.set_timeout(Some(TIMEOUT));
        sock.connect((addr, 80))
            .await
            .map_err(|e| format!("control connect: {e:?}"))?;
        let cc = ControlConn::new(HOST, machine_secret, control_pub, CAPABILITY_VERSION);
        let mut io = ControlIo { sock, cc };
        io.flush().await?;
        while !io.cc.is_ready() {
            io.process().await?;
        }
        io.flush().await?;
        Ok(io)
    }

    async fn flush(&mut self) -> Result<(), String> {
        if self.cc.has_output() {
            let out = self.cc.take_output();
            self.sock
                .write_all(&out)
                .await
                .map_err(|e| format!("control write: {e:?}"))?;
        }
        Ok(())
    }

    /// Reads once (at most [`TIMEOUT`]) and processes what came.
    async fn process(&mut self) -> Result<(), String> {
        let want = self.cc.wanted(2048);
        let r = with_timeout(TIMEOUT, self.sock.read(self.cc.read_buffer(want))).await;
        let n = match r {
            Ok(Ok(0)) => {
                let _ = self.cc.received(0);
                return Err("control connection closed".into());
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                let _ = self.cc.received(0);
                return Err(format!("control read: {e:?}"));
            }
            Err(_) => {
                let _ = self.cc.received(0);
                return Err("control read timeout".into());
            }
        };
        self.cc.received(n).map_err(|e| format!("control: {e}"))?;
        self.flush().await
    }

    /// POSTs a JSON body and returns the status and the whole response body.
    pub async fn post_json(
        &mut self,
        path: &str,
        json: &[u8],
        max: usize,
    ) -> Result<(u16, Vec<u8>), String> {
        let id = self
            .cc
            .request("POST", path, &[("content-type", "application/json")], json)
            .map_err(|e| format!("{path}: {e}"))?;
        self.flush().await?;
        while self.cc.body_unsent(id) {
            self.process().await?;
        }
        let status = loop {
            if let Some(st) = self.cc.status(id).map_err(|e| format!("{path}: {e}"))? {
                break st;
            }
            self.process().await?;
        };
        let mut body = Vec::new();
        loop {
            match self.cc.take_body(id).map_err(|e| format!("{path}: {e}"))? {
                Body::Data(d) => {
                    body.extend_from_slice(&d);
                    if body.len() > max {
                        return Err(format!("{path}: body too large"));
                    }
                }
                Body::End => break,
                Body::Pending => self.process().await?,
            }
        }
        self.cc.forget_stream(id);
        Ok((status, body))
    }

    pub fn close(mut self) {
        self.sock.close();
    }
}

/// What `probe` found out.
pub struct Probe {
    pub authorized: bool,
    pub auth_url: String,
    pub error: String,
}

/// Registers a throwaway node key without an auth key, as `hostnode
/// probe-control` does: proves DNS, TCP, the Noise handshake and HTTP/2.
pub async fn probe(stack: Stack<'static>) -> Result<Probe, String> {
    use tsnode::keys::{fmt_node_pub, KeyPair};
    use tsnode::types::{hostinfo, RegisterRequest, RegisterResponse, ZERO_TIME};

    let addr = resolve(stack, HOST).await?;
    log::info!("control: {HOST} is {addr}");
    sync_clock(stack, addr).await?;
    log::info!("control: clock set to {} (Unix)", crate::clock::now());
    let key = fetch_key(stack, addr).await?;
    log::info!("control: got the server key");
    let machine = KeyPair::generate();
    let node = KeyPair::generate();
    let mut rx = alloc::vec![0u8; 8192];
    let mut tx = alloc::vec![0u8; 4096];
    let mut io = ControlIo::open(stack, &mut rx, &mut tx, addr, machine.secret(), &key).await?;
    log::info!("control: Noise handshake done");
    let req = RegisterRequest {
        version: CAPABILITY_VERSION,
        node_key: fmt_node_pub(node.public()),
        old_node_key: fmt_node_pub(&[0u8; 32]),
        auth: None,
        expiry: ZERO_TIME.into(),
        followup: String::new(),
        hostinfo: hostinfo("tsnode-probe", env!("CARGO_PKG_VERSION"), 0),
        ephemeral: false,
    };
    let json = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    let (status, body) = io.post_json("/machine/register", &json, 64 * 1024).await?;
    io.close();
    if status != 200 {
        return Err(format!("register: HTTP {status}"));
    }
    let resp: RegisterResponse =
        serde_json::from_slice(&body).map_err(|e| format!("register: bad json: {e}"))?;
    Ok(Probe {
        authorized: resp.machine_authorized,
        auth_url: resp.auth_url,
        error: resp.error,
    })
}
