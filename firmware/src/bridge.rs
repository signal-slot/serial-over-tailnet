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

#[cfg(not(board_xiao))]
use crate::usb_host::UsbSerial;
use adapter_core::rfc2217::{Command, Purge, Session};
use adapter_core::usbserial::LineSettings;
use anyhow::Result;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tsnode::netstack::TcpStream;
use tsnode::node::Node;

/// Why a channel could not be opened for a client.
#[derive(Debug)]
pub enum OpenError {
    /// Nothing is attached (or the device has no such interface); the
    /// channel opens by itself once a device with the interface arrives.
    /// Never for a UART, which is always there.
    #[cfg_attr(board_xiao, allow(dead_code))]
    NoDevice,
    /// The interface is there but cannot be opened now, most often because
    /// the host controller has no free pipe.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NoDevice => write!(f, "no device attached"),
            OpenError::Failed(e) => write!(f, "{e}"),
        }
    }
}

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

#[cfg(not(board_xiao))]
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
    // Accepted clients are handed over in a slot this thread polls, not in a
    // `std::sync::mpsc` channel: creating one puts a value aligned to 64
    // bytes on the stack, and the Xtensa compiler realigns such a frame by
    // adding to the stack pointer without `movsp`, which can restore a
    // caller's registers from the wrong place.
    let accepted: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    let accepted2 = accepted.clone();
    crate::platform::spawn_thread(
        &format!("bridge-accept{port}"),
        crate::platform::small_ram(6 * 1024, 3 * 1024),
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
            // `busy` keeps the slot to one client.
            *accepted2.lock().unwrap() = Some(conn);
        }),
    )?;
    // Serial -> TCP, one thread for every session: a thread made and ended
    // per session leaked about 160 bytes each time (Rust's threads on
    // ESP-IDF; plain pthreads do not), which ran an ESP32-C3 out of memory
    // after some 150 connections.
    //
    // The session is handed over in a slot the thread looks at, and the end
    // is an atomic flag the session polls.
    let slot: Arc<Mutex<Option<RxJob>>> = Arc::new(Mutex::new(None));
    let slot2 = slot.clone();
    let serial2 = serial.clone();
    crate::platform::spawn_thread(
        &format!("bridge-rx{port}"),
        crate::platform::small_ram(6 * 1024, 4 * 1024),
        true,
        Box::new(move || loop {
            let job = slot2.lock().unwrap().take();
            match job {
                Some(job) => {
                    serial_to_client(&serial2, &job);
                    job.done.store(true, Ordering::Release);
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }),
    )?;
    loop {
        let conn = accepted.lock().unwrap().take();
        match conn {
            Some(conn) => {
                serve(&serial, conn, port, &slot);
                busy.store(false, Ordering::SeqCst);
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// One session's work for the serial-to-TCP thread.
struct RxJob {
    conn: Arc<TcpStream>,
    session: Arc<Mutex<Session>>,
    stop: Arc<AtomicBool>,
    /// Set when the thread is done with the session.
    done: Arc<AtomicBool>,
}

/// Sends what the target writes to the client until the session stops or
/// the client is gone.
fn serial_to_client<S: Serial>(serial: &S, job: &RxJob) {
    let mut buf = [0u8; 512];
    let mut out = Vec::with_capacity(2 * buf.len());
    while !job.stop.load(Ordering::Relaxed) {
        let n = serial.read(&mut buf, Duration::from_millis(100));
        if n > 0 {
            out.clear();
            job.session.lock().unwrap().encode(&buf[..n], &mut out);
            if job.conn.write_all(&out).is_err() {
                break;
            }
        }
        if job.conn.is_closed() {
            break;
        }
    }
}

/// Runs one client session until either side closes.
fn serve<S: Serial>(serial: &S, conn: TcpStream, port: u16, rx_slot: &Mutex<Option<RxJob>>) {
    let peer = conn.peer_addr();
    info!("bridge: client connected from {peer} on {port}");
    match serial.open() {
        Ok(()) => {}
        Err(OpenError::NoDevice) => info!("bridge: {peer} on {port} waits for a device"),
        Err(OpenError::Failed(e)) => {
            warn!("bridge: rejecting {peer} on {port}: {e}");
            let _ = conn.write_all(format!("serial: cannot open the channel: {e}\r\n").as_bytes());
            conn.close();
            return;
        }
    }
    let conn = Arc::new(conn);
    // The session is decided by the client's first byte; until then bytes
    // from the target go out as they are (console output has no IAC).
    let session = Arc::new(Mutex::new(Session::new(
        serial.line(),
        &format!("serial-over-tailnet {}", crate::FIRMWARE_VERSION),
    )));
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    *rx_slot.lock().unwrap() = Some(RxJob {
        conn: conn.clone(),
        session: session.clone(),
        stop: stop.clone(),
        done: done.clone(),
    });
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
    // The thread may not have taken the job yet; then it is taken back.
    if rx_slot.lock().unwrap().take().is_none() {
        while !done.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    conn.close();
    serial.close();
    info!("bridge: client {peer} on {port} disconnected");
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
