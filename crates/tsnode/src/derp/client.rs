//! Runs a [`DerpSession`] over a blocking TLS connection (`std`).

use super::*;
use crate::keys::short;
use crate::net::{is_timeout, Conn, Dialer};
use crate::types::DerpNode;
use core::net::IpAddr;
use log::debug;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

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
