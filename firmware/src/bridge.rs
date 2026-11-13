//! Bridge between one TCP client on the tailnet and the USB serial
//! console. Bytes are forwarded unchanged in both directions, unless the
//! client opens with Telnet negotiation: then the session speaks RFC 2217
//! and the client can set the baud rate, the framing and the DTR and RTS
//! lines (see `adapter_core::rfc2217`).
//!
//! Accepting runs on its own thread so that a second client is told the
//! bridge is busy right away instead of waiting in the backlog.
//!
//! One bridge serves one channel. A device with several serial interfaces (an
//! FT4232H has four) has one bridge per interface, each on its own port, and
//! "busy" is per bridge: a client on one port does not block the other. The
//! USB interface is claimed at the first session and kept until the device
//! leaves, so that the device's data toggles stay in step with the host's
//! pipes (see `usb_host`).
//!
//! The channel is anything that implements [`Serial`]: an interface of the
//! USB device, or the adapter board's UART header (`uart_link`).

use crate::usb_host::{OpenError, UsbSerial};
use adapter_core::rfc2217::{Command, Purge, Session};
use adapter_core::usbserial::LineSettings;
use anyhow::{anyhow, Result};
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tsnode::netstack::TcpStream;
use tsnode::node::Node;

/// A serial channel a bridge can serve. The methods are those of
/// [`UsbSerial`]; see there for what each does.
pub trait Serial: Clone + Send + Sync + 'static {
    fn open(&self) -> std::result::Result<(), OpenError>;
    fn close(&self);
    fn line(&self) -> LineSettings;
    fn set_line(&self, line: LineSettings) -> Result<()>;
    /// DTR and RTS.
    fn modem(&self) -> (bool, bool);
    fn set_modem(&self, dtr: bool, rts: bool) -> Result<()>;
    fn set_break(&self, on: bool) -> Result<()>;
    fn purge_rx(&self);
    /// Bytes from the target; 0 on timeout.
    fn read(&self, buf: &mut [u8], timeout: Duration) -> usize;
    fn write(&self, data: &[u8]) -> Result<()>;
}

impl Serial for UsbSerial {
    fn open(&self) -> std::result::Result<(), OpenError> {
        UsbSerial::open(self)
    }
    fn close(&self) {
        UsbSerial::close(self)
    }
    fn line(&self) -> LineSettings {
        UsbSerial::line(self)
    }
    fn set_line(&self, line: LineSettings) -> Result<()> {
        UsbSerial::set_line(self, line)
    }
    fn modem(&self) -> (bool, bool) {
        UsbSerial::modem(self)
    }
    fn set_modem(&self, dtr: bool, rts: bool) -> Result<()> {
        UsbSerial::set_modem(self, dtr, rts)
    }
    fn set_break(&self, on: bool) -> Result<()> {
        UsbSerial::set_break(self, on)
    }
    fn purge_rx(&self) {
        UsbSerial::purge_rx(self)
    }
    fn read(&self, buf: &mut [u8], timeout: Duration) -> usize {
        UsbSerial::read(self, buf, timeout)
    }
    fn write(&self, data: &[u8]) -> Result<()> {
        UsbSerial::write(self, data)
    }
}

pub fn run<S: Serial>(node: Node, serial: S, port: u16) -> Result<()> {
    let listener = node.listen(port);
    info!("bridge: listening on tailnet port {port}");
    let busy = Arc::new(AtomicBool::new(false));
    let accept_busy = busy.clone();
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(1);
    crate::platform::spawn_thread(
        &format!("bridge-accept{port}"),
        6 * 1024,
        true,
        Box::new(move || loop {
            let Some(conn) = listener.accept(Duration::from_secs(1)) else {
                continue;
            };
            if accept_busy.swap(true, Ordering::SeqCst) {
                warn!("bridge: rejecting {} on {port} (busy)", conn.peer_addr());
                let _ = conn.write_all(b"busy: another client is connected\r\n");
                conn.close();
                continue;
            }
            if tx.send(conn).is_err() {
                break;
            }
        }),
    )?;
    loop {
        let conn = rx
            .recv()
            .map_err(|_| anyhow!("bridge: accept thread exited"))?;
        serve(&serial, conn, port)?;
        busy.store(false, Ordering::SeqCst);
    }
}

/// Runs one client session until either side closes.
fn serve<S: Serial>(serial: &S, conn: TcpStream, port: u16) -> Result<()> {
    let peer = conn.peer_addr();
    info!("bridge: client connected from {peer} on {port}");
    match serial.open() {
        Ok(()) => {}
        Err(OpenError::NoDevice) => info!("bridge: {peer} on {port} waits for a device"),
        Err(OpenError::Failed(e)) => {
            warn!("bridge: rejecting {peer} on {port}: {e}");
            let _ = conn.write_all(format!("serial: cannot open the channel: {e}\r\n").as_bytes());
            conn.close();
            return Ok(());
        }
    }
    let conn = Arc::new(conn);
    // The session is decided by the client's first byte; until then bytes
    // from the target go out as they are (console output has no IAC).
    let session = Arc::new(Mutex::new(Session::new(
        serial.line(),
        &format!("serial-over-tailnet {}", crate::FIRMWARE_VERSION),
    )));
    let serial2 = serial.clone();
    let c2 = conn.clone();
    let session2 = session.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    // Serial -> TCP
    let rx_done = Arc::new(AtomicBool::new(false));
    let rx_done2 = rx_done.clone();
    crate::platform::spawn_thread(
        &format!("bridge-rx{port}"),
        6 * 1024,
        true,
        Box::new(move || {
            let mut buf = [0u8; 512];
            let mut out = Vec::with_capacity(2 * buf.len());
            while !stop2.load(Ordering::Relaxed) {
                let n = serial2.read(&mut buf, Duration::from_millis(100));
                if n > 0 {
                    out.clear();
                    session2.lock().unwrap().encode(&buf[..n], &mut out);
                    if c2.write_all(&out).is_err() {
                        break;
                    }
                }
                if c2.is_closed() {
                    break;
                }
            }
            rx_done2.store(true, Ordering::Relaxed);
        }),
    )?;
    // TCP -> Serial
    let mut buf = [0u8; 512];
    let mut to_serial = Vec::with_capacity(buf.len());
    let mut to_client = Vec::with_capacity(64);
    let mut commands = Vec::new();
    let mut dropped_warned = false;
    loop {
        match conn.read(&mut buf, Duration::from_millis(100)) {
            Ok(0) => break,
            Ok(n) => {
                to_serial.clear();
                to_client.clear();
                commands.clear();
                let telnet_before;
                {
                    let mut s = session.lock().unwrap();
                    telnet_before = s.is_telnet();
                    s.feed(&buf[..n], &mut to_serial, &mut to_client, &mut commands);
                    if !telnet_before && s.is_telnet() {
                        info!("bridge: {peer} on {port} speaks Telnet (RFC 2217)");
                    }
                }
                for &command in &commands {
                    let applied = apply(serial, command, peer, port);
                    session
                        .lock()
                        .unwrap()
                        .complete(command, applied, &mut to_client);
                }
                if !to_client.is_empty() && conn.write_all(&to_client).is_err() {
                    break;
                }
                if to_serial.is_empty() {
                    continue;
                }
                if let Err(e) = serial.write(&to_serial) {
                    if !dropped_warned {
                        warn!("bridge: dropping input, {e}");
                        dropped_warned = true;
                    }
                } else {
                    dropped_warned = false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    while !rx_done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
    conn.close();
    serial.close();
    info!("bridge: client {peer} on {port} disconnected");
    Ok(())
}

/// Applies one RFC 2217 request to the serial port; `false` when the port
/// refused it, which the client is then told.
fn apply<S: Serial>(serial: &S, command: Command, peer: std::net::SocketAddr, port: u16) -> bool {
    let result = match command {
        Command::Baud(baud) => {
            let mut line = serial.line();
            line.baud = baud;
            serial.set_line(line)
        }
        Command::DataBits(bits) => {
            let mut line = serial.line();
            line.data_bits = bits;
            serial.set_line(line)
        }
        Command::Parity(parity) => {
            let mut line = serial.line();
            line.parity = parity;
            serial.set_line(line)
        }
        Command::StopBits(stop_bits) => {
            let mut line = serial.line();
            line.stop_bits = stop_bits;
            serial.set_line(line)
        }
        Command::Dtr(on) => {
            let (_, rts) = serial.modem();
            serial.set_modem(on, rts)
        }
        Command::Rts(on) => {
            let (dtr, _) = serial.modem();
            serial.set_modem(dtr, on)
        }
        Command::Break(on) => serial.set_break(on),
        Command::Purge(purge) => {
            // Writes are not buffered here, so only the receive side has
            // anything to drop.
            if purge != Purge::Transmit {
                serial.purge_rx();
            }
            Ok(())
        }
    };
    match result {
        Ok(()) => {
            info!("bridge: {peer} on {port}: {command:?}");
            true
        }
        Err(e) => {
            warn!("bridge: {peer} on {port}: {command:?} refused: {e}");
            false
        }
    }
}
