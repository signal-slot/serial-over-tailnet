//! TLS over an embassy-net TCP socket: rustls's unbuffered client (the only
//! one it has without std), with RustCrypto's algorithms, Mozilla's root
//! certificates and the time from [`crate::clock`].

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration};
use embedded_io_async::Write;
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::ServerName;
use rustls::unbuffered::{
    ConnectionState, EncodeError, EncryptError, InsufficientSizeError, UnbufferedStatus,
};
use rustls::{ClientConfig, RootCertStore};

/// Room for one whole TLS record (16 KB of payload plus its overhead).
const RECORD: usize = 16 * 1024 + 512;
/// The most plaintext put into one record when sending.
const CHUNK: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(30);

/// The client settings, built once: the root store is large.
fn config() -> Result<Arc<ClientConfig>, String> {
    static CONFIG: critical_section::Mutex<core::cell::RefCell<Option<Arc<ClientConfig>>>> =
        critical_section::Mutex::new(core::cell::RefCell::new(None));
    if let Some(c) = critical_section::with(|cs| CONFIG.borrow_ref(cs).clone()) {
        return Ok(c);
    }
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let c = ClientConfig::builder_with_details(
        Arc::new(rustls_rustcrypto::provider()),
        Arc::new(crate::clock::Clock),
    )
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("tls config: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let c = Arc::new(c);
    critical_section::with(|cs| *CONFIG.borrow_ref_mut(cs) = Some(c.clone()));
    Ok(c)
}

/// What one turn of the state machine asks for next.
enum Next {
    /// Call `turn` again.
    Again,
    /// Send what is in `out`.
    Transmit,
    /// Read more from the socket.
    Read,
    /// The connection can carry application data (and `send` was sent).
    Writable,
    /// The peer closed the connection.
    Closed,
}

pub struct TlsStream<'a> {
    sock: TcpSocket<'a>,
    conn: UnbufferedClientConnection,
    /// Received TLS bytes not yet processed.
    inc: Vec<u8>,
    inc_len: usize,
    /// TLS bytes to send.
    out: Vec<u8>,
    out_len: usize,
    /// Decrypted bytes not yet read.
    plain: Vec<u8>,
    eof: bool,
}

impl<'a> TlsStream<'a> {
    /// Runs the handshake with `host` over the connected `sock`.
    pub async fn connect(sock: TcpSocket<'a>, host: &str) -> Result<Self, String> {
        let name = ServerName::try_from(String::from(host))
            .map_err(|e| format!("tls: bad name {host}: {e}"))?;
        let conn =
            UnbufferedClientConnection::new(config()?, name).map_err(|e| format!("tls: {e}"))?;
        let mut s = TlsStream {
            sock,
            conn,
            inc: vec![0; RECORD],
            inc_len: 0,
            out: vec![0; RECORD],
            out_len: 0,
            plain: Vec::new(),
            eof: false,
        };
        loop {
            match s.turn(None).await? {
                Next::Writable => return Ok(s),
                Next::Closed => return Err("tls: closed in the handshake".into()),
                _ => {}
            }
        }
    }

    /// One call into rustls, then the I/O it asks for. With `send`, the
    /// data is encrypted and sent once the connection is writable.
    async fn turn(&mut self, send: Option<&[u8]>) -> Result<Next, String> {
        let UnbufferedStatus { mut discard, state } =
            self.conn.process_tls_records(&mut self.inc[..self.inc_len]);
        let next = match state.map_err(|e| format!("tls: {e}"))? {
            ConnectionState::EncodeTlsData(mut st) => loop {
                match st.encode(&mut self.out[self.out_len..]) {
                    Ok(n) => {
                        self.out_len += n;
                        break Next::Again;
                    }
                    Err(EncodeError::InsufficientSize(InsufficientSizeError { required_size })) => {
                        self.out.resize(self.out_len + required_size, 0)
                    }
                    Err(e) => return Err(format!("tls: {e}")),
                }
            },
            ConnectionState::TransmitTlsData(st) => {
                // `out` is sent below, once rustls's borrows have ended.
                st.done();
                Next::Transmit
            }
            ConnectionState::BlockedHandshake => Next::Read,
            ConnectionState::WriteTraffic(mut st) => {
                if let Some(data) = send {
                    for chunk in data.chunks(CHUNK) {
                        loop {
                            match st.encrypt(chunk, &mut self.out[self.out_len..]) {
                                Ok(n) => {
                                    self.out_len += n;
                                    break;
                                }
                                Err(EncryptError::InsufficientSize(InsufficientSizeError {
                                    required_size,
                                })) => self.out.resize(self.out_len + required_size, 0),
                                Err(e) => return Err(format!("tls: {e}")),
                            }
                        }
                    }
                }
                Next::Writable
            }
            ConnectionState::ReadTraffic(mut st) => {
                while let Some(rec) = st.next_record() {
                    let rec = rec.map_err(|e| format!("tls: {e}"))?;
                    discard += rec.discard;
                    self.plain.extend_from_slice(rec.payload);
                }
                Next::Again
            }
            ConnectionState::PeerClosed | ConnectionState::Closed => Next::Closed,
            _ => return Err("tls: unexpected state".into()),
        };
        self.inc.copy_within(discard..self.inc_len, 0);
        self.inc_len -= discard;
        match next {
            Next::Transmit => self.flush().await?,
            Next::Read => {
                self.fill().await?;
                if self.eof {
                    return Err("tls: closed in the handshake".into());
                }
            }
            Next::Writable if send.is_some() => self.flush().await?,
            Next::Closed => self.eof = true,
            _ => {}
        }
        Ok(next)
    }

    async fn flush(&mut self) -> Result<(), String> {
        if self.out_len > 0 {
            self.sock
                .write_all(&self.out[..self.out_len])
                .await
                .map_err(|e| format!("tls write: {e:?}"))?;
            self.out_len = 0;
        }
        Ok(())
    }

    /// Reads from the socket into `inc`; the end of the stream ends it.
    async fn fill(&mut self) -> Result<(), String> {
        if self.inc_len == self.inc.len() {
            return Err("tls: record too large".into());
        }
        let r = with_timeout(TIMEOUT, self.sock.read(&mut self.inc[self.inc_len..])).await;
        match r {
            Ok(Ok(0)) => {
                self.eof = true;
                Ok(())
            }
            Ok(Ok(n)) => {
                self.inc_len += n;
                Ok(())
            }
            Ok(Err(e)) => Err(format!("tls read: {e:?}")),
            Err(_) => Err("tls read timeout".into()),
        }
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        loop {
            match self.turn(Some(data)).await? {
                Next::Writable => return Ok(()),
                Next::Closed => return Err("tls: closed".into()),
                _ => {}
            }
        }
    }

    /// Reads decrypted bytes; 0 at the end of the stream.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        while self.plain.is_empty() {
            match self.turn(None).await? {
                Next::Closed => break,
                // Every whole record is decrypted: wait for more.
                Next::Writable if self.eof => break,
                Next::Writable => self.fill().await?,
                _ => {}
            }
        }
        let n = buf.len().min(self.plain.len());
        buf[..n].copy_from_slice(&self.plain[..n]);
        self.plain.drain(..n);
        Ok(n)
    }

    pub fn close(mut self) {
        self.sock.close();
    }
}
