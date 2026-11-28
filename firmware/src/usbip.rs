//! USB/IP server: the device on the board's USB port is exported whole, on
//! tailnet port 3240, instead of its serial interfaces being bridged. The
//! PC attaches it with `usbip attach` and its own drivers and tools (a debug
//! probe's CMSIS-DAP, J-Link or vendor protocol, its serial port) talk to it
//! as if it were plugged into the PC.
//!
//! The protocol is encoded and decoded in `adapter_core::usbip`; here the
//! URBs become ESP-IDF USB host transfers. Threads, made once:
//!
//! - `usb-events` runs the host library.
//! - `usbip-client` runs this client's events: a device arriving or leaving,
//!   and every transfer's completion callback, which queues the reply.
//! - `usbip` accepts connections and reads the client's commands.
//! - `usbip-tx` writes the queued replies.
//!
//! One client at a time imports the device. A URB's interface is claimed
//! the first time one of its endpoints is used, so that the host's eight
//! pipes go only to what the client actually opens. The requests that change
//! what the board's host keeps (SET_CONFIGURATION, SET_INTERFACE, a halt
//! cleared, a port reset) are carried out here instead of being passed on.
//!
//! ESP-IDF cannot cancel one transfer: an unlinked URB's endpoint is halted
//! and flushed, and the other URBs that were queued on it are submitted
//! again in their order.

use adapter_core::usbdesc;
use adapter_core::usbip::{self, status, Command, Control, DeviceInfo, OpRequest, Speed, Submit};
use anyhow::{anyhow, Result};
use esp_idf_svc::sys::{self, usb};
use log::{debug, info, warn};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tsnode::netstack::TcpStream;
use tsnode::node::Node;

/// The largest URB accepted; its buffer comes out of the internal RAM.
const MAX_URB: u32 = 32 * 1024;
/// Buffers of the URBs in flight together; a submit past this waits.
const MAX_INFLIGHT_BYTES: usize = 64 * 1024;
/// The one device's bus ID, as the PC names it in `usbip attach -b`.
pub const BUSID: &str = "1-1";
/// Linux `URB_ZERO_PACKET`: end an OUT transfer of whole packets with a
/// zero-length one.
const URB_ZERO_PACKET: u32 = 0x0040;
/// How long a cancellation or an internal control transfer may take.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct DevHandle(usb::usb_device_handle_t);
unsafe impl Send for DevHandle {}

#[derive(Clone, Copy)]
struct ClientHandle(usb::usb_host_client_handle_t);
unsafe impl Send for ClientHandle {}
unsafe impl Sync for ClientHandle {}

struct Dev {
    handle: DevHandle,
    info: DeviceInfo,
    config: Vec<u8>,
    mps0: u16,
    /// Interfaces claimed from the host library, with the alternate
    /// setting they were claimed in.
    claimed: BTreeMap<u8, u8>,
    /// Alternate settings the client selected (absent: 0).
    alt: BTreeMap<u8, u8>,
    /// CMSIS-DAP interfaces asked for their packet size, with the size of
    /// their bulk OUT packets: the answer is capped to it.
    dap_info: BTreeMap<u8, u16>,
}

impl Dev {
    fn alt_of(&self, interface: u8) -> u8 {
        self.alt.get(&interface).copied().unwrap_or(0)
    }

    fn endpoints_of(&self, interface: u8) -> Vec<u8> {
        usbdesc::endpoints(&self.config, interface, self.alt_of(interface))
            .iter()
            .map(|e| e.address)
            .collect()
    }
}

/// A transfer in flight, keyed by its `usb_transfer_t` pointer.
struct Urb {
    /// The session it belongs to; a completion for an older one is dropped.
    session: u32,
    seqnum: u32,
    /// Endpoint address (0x00 or 0x80 for a control transfer).
    ep: u8,
    dir_in: bool,
    control: bool,
    /// The client's buffer length: IN data past it is an overflow.
    requested: u32,
    /// Buffer size, counted against `MAX_INFLIGHT_BYTES`.
    bytes: usize,
    /// The client unlinked it; the value is the unlink's own seqnum.
    unlink: Option<u32>,
    /// Cancelled only to make way for an unlink or a reset on its endpoint:
    /// submitted again afterwards.
    park: bool,
    /// Sent by the server itself; the waiting thread takes the result.
    internal: bool,
}

#[derive(Default)]
struct State {
    dev: Option<Dev>,
    /// A device the host library announced, for the client thread to open.
    open_address: Option<u8>,
    /// The device left; it is closed once nothing uses it.
    gone: bool,
    /// The current session (0: none) and the last one given out.
    session: u32,
    last_session: u32,
    inflight: BTreeMap<usize, Urb>,
    parked: Vec<(usize, Urb)>,
    /// Status and actual length of finished internal transfers.
    internal_done: BTreeMap<usize, (u32, usize)>,
    replies: VecDeque<Vec<u8>>,
    inflight_bytes: usize,
    conn: Option<Arc<TcpStream>>,
    /// The session cannot go on (a write failed, the device left).
    broken: bool,
}

pub struct UsbIp {
    state: Mutex<State>,
    cv: Condvar,
    client: OnceLock<ClientHandle>,
}

static SERVER: OnceLock<&'static UsbIp> = OnceLock::new();

fn ticks(d: Duration) -> u32 {
    (d.as_millis() as u64 * sys::configTICK_RATE_HZ as u64 / 1000).max(1) as u32
}

fn esp_check(rc: sys::esp_err_t, what: &str) -> Result<()> {
    if rc == sys::ESP_OK {
        Ok(())
    } else {
        Err(anyhow!("{what} failed: esp_err {rc}"))
    }
}

fn round_up(n: usize, to: usize) -> usize {
    if to == 0 {
        n
    } else {
        n.div_ceil(to) * to
    }
}

unsafe extern "C" fn client_cb(msg: *const usb::usb_host_client_event_msg_t, _arg: *mut c_void) {
    let Some(server) = SERVER.get() else { return };
    let msg = &*msg;
    let mut st = server.state.lock().unwrap();
    match msg.event {
        usb::usb_host_client_event_t_USB_HOST_CLIENT_EVENT_NEW_DEV => {
            st.open_address = Some(msg.__bindgen_anon_1.new_dev.address);
        }
        usb::usb_host_client_event_t_USB_HOST_CLIENT_EVENT_DEV_GONE => {
            st.gone = true;
            if st.session != 0 {
                st.broken = true;
            }
        }
        _ => {}
    }
    server.cv.notify_all();
}

/// Runs on the client thread for every finished transfer.
unsafe extern "C" fn transfer_cb(t: *mut usb::usb_transfer_t) {
    let Some(server) = SERVER.get() else { return };
    let tr = &*t;
    let key = t as usize;
    let mut st = server.state.lock().unwrap();
    let Some(urb) = st.inflight.remove(&key) else {
        // Not ours to account for (cannot happen); do not leak it.
        drop(st);
        usb::usb_host_transfer_free(t);
        return;
    };
    let actual = tr.actual_num_bytes.max(0) as usize;
    if urb.internal {
        st.internal_done.insert(key, (tr.status, actual));
        server.cv.notify_all();
        return;
    }
    let cancelled = tr.status == usb::usb_transfer_status_t_USB_TRANSFER_STATUS_CANCELED;
    if urb.session == st.session && cancelled && urb.park && urb.unlink.is_none() && actual == 0 {
        st.parked.push((key, urb));
        server.cv.notify_all();
        return;
    }
    st.inflight_bytes -= urb.bytes;
    if urb.session == st.session {
        let data_actual = if urb.control {
            actual.saturating_sub(8)
        } else {
            actual
        };
        match urb.unlink {
            Some(unlink) if cancelled => {
                st.replies
                    .push_back(usbip::ret_unlink(unlink, status::ECONNRESET));
            }
            unlink => {
                if urb.dir_in && !urb.control {
                    cap_dap_reply(&mut st, urb.ep, tr.data_buffer, actual);
                }
                let (code, len) = reply_status(tr.status, data_actual, urb.requested as usize);
                let data: &[u8] = if urb.dir_in && len > 0 {
                    let start = if urb.control { 8 } else { 0 };
                    std::slice::from_raw_parts(tr.data_buffer.add(start), len)
                } else {
                    &[]
                };
                st.replies
                    .push_back(usbip::ret_submit(urb.seqnum, code, len as u32, data));
                if let Some(unlink) = unlink {
                    // It finished before it could be cancelled.
                    st.replies.push_back(usbip::ret_unlink(unlink, status::OK));
                }
            }
        }
    }
    drop(st);
    usb::usb_host_transfer_free(t);
    server.cv.notify_all();
}

/// The first bulk IN data after a CMSIS-DAP packet size request is its
/// answer: the size is capped to the endpoint's packets
/// (`usbip::cap_dap_packet_size`).
fn cap_dap_reply(st: &mut State, ep: u8, buf: *mut u8, len: usize) {
    let Some(d) = st.dev.as_mut() else { return };
    if d.dap_info.is_empty() {
        return;
    }
    let Some((interface, _)) = usbdesc::interface_of(&d.config, ep, |i| d.alt_of(i)) else {
        return;
    };
    let Some(max) = d.dap_info.remove(&interface) else {
        return;
    };
    // SAFETY: the transfer's buffer holds `len` received bytes and nothing
    // else uses it until the transfer is freed.
    let reply = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    if usbip::cap_dap_packet_size(reply, max) {
        info!("usbip: CMSIS-DAP on interface {interface} announced packets larger than its {max}-byte endpoint; capped");
    }
}

/// The URB status and the length to report for a finished transfer.
fn reply_status(st: u32, actual: usize, requested: usize) -> (i32, usize) {
    match st {
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED if actual > requested => {
            (status::EOVERFLOW, requested)
        }
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED => (status::OK, actual),
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_STALL => {
            (status::EPIPE, actual.min(requested))
        }
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_NO_DEVICE => (status::ENODEV, 0),
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_TIMED_OUT => {
            (status::ETIME, actual.min(requested))
        }
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_OVERFLOW => {
            (status::EOVERFLOW, actual.min(requested))
        }
        usb::usb_transfer_status_t_USB_TRANSFER_STATUS_CANCELED => {
            (status::ECONNRESET, actual.min(requested))
        }
        _ => (status::EPROTO, actual.min(requested)),
    }
}

/// What the PC sees in `status`.
pub struct UsbIpHandle(&'static UsbIp);

impl UsbIpHandle {
    pub fn describe(&self) -> String {
        let st = self.0.state.lock().unwrap();
        match &st.dev {
            None => format!("USB/IP on port {}: no device", usbip::USBIP_PORT),
            Some(d) => format!(
                "USB/IP on port {}: {:04x}:{:04x} as {BUSID}{}",
                usbip::USBIP_PORT,
                d.info.vid,
                d.info.pid,
                match (&st.conn, st.session) {
                    (Some(c), s) if s != 0 => format!(", attached by {}", c.peer_addr().ip()),
                    _ => String::new(),
                }
            ),
        }
    }
}

/// Installs the USB host library, registers the client and starts the
/// server on the node's port 3240.
pub fn start(node: Node) -> Result<UsbIpHandle> {
    let server: &'static UsbIp = Box::leak(Box::new(UsbIp {
        state: Mutex::new(State::default()),
        cv: Condvar::new(),
        client: OnceLock::new(),
    }));
    if SERVER.set(server).is_err() {
        return Err(anyhow!("usbip already started"));
    }
    crate::usb_host::install_host()?;
    let client = unsafe {
        let mut cfg: usb::usb_host_client_config_t = core::mem::zeroed();
        cfg.is_synchronous = false;
        cfg.max_num_event_msg = 5;
        cfg.__bindgen_anon_1.async_.client_event_callback = Some(client_cb);
        cfg.__bindgen_anon_1.async_.callback_arg = core::ptr::null_mut();
        let mut h: usb::usb_host_client_handle_t = core::ptr::null_mut();
        esp_check(
            usb::usb_host_client_register(&cfg, &mut h),
            "usb_host_client_register",
        )?;
        ClientHandle(h)
    };
    let _ = server.client.set(client);
    crate::platform::spawn_thread(
        "usbip-client",
        6 * 1024,
        false,
        Box::new(move || server.client_loop()),
    )?;
    crate::platform::spawn_thread(
        "usbip-tx",
        6 * 1024,
        true,
        Box::new(move || server.write_loop()),
    )?;
    crate::platform::spawn_thread(
        "usbip",
        8 * 1024,
        true,
        Box::new(move || server.serve(node)),
    )?;
    info!(
        "usbip: exporting the USB port's device on port {} as bus ID {BUSID}",
        usbip::USBIP_PORT
    );
    Ok(UsbIpHandle(server))
}

impl UsbIp {
    fn client(&self) -> usb::usb_host_client_handle_t {
        self.client.get().map_or(core::ptr::null_mut(), |c| c.0)
    }

    /// Waits on the condition variable until `done` holds or `timeout`
    /// passes; returns whether it holds.
    fn wait_for<'a>(
        &self,
        mut st: MutexGuard<'a, State>,
        timeout: Duration,
        done: impl Fn(&State) -> bool,
    ) -> (MutexGuard<'a, State>, bool) {
        let deadline = Instant::now() + timeout;
        loop {
            if done(&st) {
                return (st, true);
            }
            let now = Instant::now();
            if now >= deadline {
                return (st, false);
            }
            st = self.cv.wait_timeout(st, deadline - now).unwrap().0;
        }
    }

    // ---------------------------------------------------------------- device

    fn client_loop(&self) {
        loop {
            unsafe {
                usb::usb_host_client_handle_events(
                    self.client(),
                    ticks(Duration::from_millis(100)),
                );
            }
            let mut st = self.state.lock().unwrap();
            if let Some(address) = st.open_address.take() {
                drop(st);
                if let Err(e) = self.open(address) {
                    warn!("usbip: device at address {address}: {e:#}");
                }
                st = self.state.lock().unwrap();
            }
            let idle = st.session == 0
                && st.inflight.is_empty()
                && st.dev.as_ref().is_some_and(|d| d.claimed.is_empty());
            if st.gone && idle {
                if let Some(d) = st.dev.take() {
                    unsafe { usb::usb_host_device_close(self.client(), d.handle.0) };
                    info!("usbip: device {:04x}:{:04x} gone", d.info.vid, d.info.pid);
                }
                st.gone = false;
            } else if st.gone && st.dev.is_none() {
                st.gone = false;
            }
        }
    }

    fn open(&self, address: u8) -> Result<()> {
        let client = self.client();
        let mut h: usb::usb_device_handle_t = core::ptr::null_mut();
        unsafe {
            esp_check(
                usb::usb_host_device_open(client, address, &mut h),
                "usb_host_device_open",
            )?;
        }
        let read = || -> Result<Dev> {
            unsafe {
                let mut desc: *const usb::usb_device_desc_t = core::ptr::null();
                esp_check(
                    usb::usb_host_get_device_descriptor(h, &mut desc),
                    "device descriptor",
                )?;
                let device = std::slice::from_raw_parts(desc as *const u8, 18).to_vec();
                let mut cfg: *const usb::usb_config_desc_t = core::ptr::null();
                esp_check(
                    usb::usb_host_get_active_config_descriptor(h, &mut cfg),
                    "configuration descriptor",
                )?;
                let total = (*cfg).__bindgen_anon_1.wTotalLength as usize;
                let config = std::slice::from_raw_parts(cfg as *const u8, total).to_vec();
                let mut di: usb::usb_device_info_t = Default::default();
                esp_check(usb::usb_host_device_info(h, &mut di), "device info")?;
                let speed = match di.speed {
                    usb::usb_speed_t_USB_SPEED_LOW => Speed::Low,
                    usb::usb_speed_t_USB_SPEED_HIGH => Speed::High,
                    _ => Speed::Full,
                };
                let info =
                    DeviceInfo::from_descriptors(BUSID, address as u32, speed, &device, &config)
                        .ok_or_else(|| anyhow!("unreadable descriptors"))?;
                Ok(Dev {
                    handle: DevHandle(h),
                    info,
                    config,
                    mps0: di.bMaxPacketSize0 as u16,
                    claimed: BTreeMap::new(),
                    alt: BTreeMap::new(),
                    dap_info: BTreeMap::new(),
                })
            }
        };
        match read() {
            Ok(dev) => {
                info!(
                    "usbip: device {:04x}:{:04x} class {:02x}, {} interfaces, ready as bus ID {BUSID}",
                    dev.info.vid,
                    dev.info.pid,
                    dev.info.class,
                    dev.info.interfaces.len()
                );
                let mut st = self.state.lock().unwrap();
                if let Some(old) = st.dev.replace(dev) {
                    warn!(
                        "usbip: a second device replaced {:04x}:{:04x}",
                        old.info.vid, old.info.pid
                    );
                }
                st.gone = false;
                Ok(())
            }
            Err(e) => {
                unsafe { usb::usb_host_device_close(client, h) };
                Err(e)
            }
        }
    }

    // --------------------------------------------------------------- network

    fn serve(&'static self, node: Node) {
        let listener = node.listen(usbip::USBIP_PORT);
        loop {
            let Some(conn) = listener.accept(Duration::from_secs(1)) else {
                continue;
            };
            self.connection(Arc::new(conn));
        }
    }

    fn connection(&self, conn: Arc<TcpStream>) {
        let mut h = [0u8; usbip::OP_HEADER_LEN];
        if read_exact(&conn, &mut h, Some(Duration::from_secs(10)), || false).is_err() {
            return;
        }
        let present = || {
            let st = self.state.lock().unwrap();
            st.dev.as_ref().filter(|_| !st.gone).map(|d| d.info.clone())
        };
        match usbip::parse_op(&h) {
            OpRequest::DevList => {
                let _ = conn.write_all(&usbip::rep_devlist(present().as_ref()));
                conn.close();
            }
            OpRequest::Import => {
                let mut b = [0u8; usbip::BUSID_LEN];
                if read_exact(&conn, &mut b, Some(Duration::from_secs(10)), || false).is_err() {
                    return;
                }
                let busid = usbip::parse_busid(&b);
                let info = present().filter(|i| i.busid == busid);
                if info.is_none() {
                    info!(
                        "usbip: {} asked for bus ID {busid:?}, which is not here",
                        conn.peer_addr()
                    );
                }
                let _ = conn.write_all(&usbip::rep_import(info.as_ref()));
                match info {
                    Some(info) => self.session(conn, info),
                    None => conn.close(),
                }
            }
            OpRequest::Unknown { version, code } => {
                warn!(
                    "usbip: {} sent operation {code:04x} version {version:04x}",
                    conn.peer_addr()
                );
                conn.close();
            }
        }
    }

    fn session(&self, conn: Arc<TcpStream>, info: DeviceInfo) {
        let session = {
            let mut st = self.state.lock().unwrap();
            st.last_session = st.last_session.wrapping_add(1).max(1);
            st.session = st.last_session;
            st.conn = Some(conn.clone());
            st.broken = false;
            st.replies.clear();
            st.session
        };
        info!(
            "usbip: {} attached {:04x}:{:04x}",
            conn.peer_addr(),
            info.vid,
            info.pid
        );
        let broken = || self.state.lock().unwrap().broken;
        loop {
            let mut h = [0u8; usbip::URB_HEADER_LEN];
            if read_exact(&conn, &mut h, None, broken).is_err() {
                break;
            }
            match usbip::parse_command(&h, MAX_URB) {
                Ok(Command::Submit(s)) => {
                    let mut trailer = vec![0u8; s.trailer_len()];
                    if read_exact(&conn, &mut trailer, None, broken).is_err() {
                        break;
                    }
                    let out_len = if s.dir_in { 0 } else { s.length as usize };
                    trailer.truncate(out_len);
                    if s.devid != info.devid() {
                        self.reply(session, usbip::ret_submit(s.seqnum, status::ENODEV, 0, &[]));
                    } else if s.packets > 0 {
                        // Isochronous: nothing this is meant for uses it.
                        self.reply(session, usbip::ret_submit(s.seqnum, status::EINVAL, 0, &[]));
                    } else {
                        self.submit_urb(session, s, trailer);
                    }
                }
                Ok(Command::Unlink { seqnum, victim, .. }) => self.unlink(session, seqnum, victim),
                Err(e) => {
                    warn!("usbip: bad command from {}: {e:?}", conn.peer_addr());
                    break;
                }
            }
        }
        self.end_session();
        conn.close();
        info!("usbip: {} detached", conn.peer_addr());
    }

    fn reply(&self, session: u32, r: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        if st.session == session {
            st.replies.push_back(r);
            self.cv.notify_all();
        }
    }

    fn write_loop(&self) {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.replies.is_empty() || st.conn.is_none() {
                st = self
                    .cv
                    .wait_timeout(st, Duration::from_millis(200))
                    .unwrap()
                    .0;
                continue;
            }
            let mut out = Vec::new();
            while let Some(r) = st.replies.pop_front() {
                out.extend_from_slice(&r);
                if out.len() >= 16 * 1024 {
                    break;
                }
            }
            let conn = st.conn.clone().expect("checked above");
            let session = st.session;
            drop(st);
            let ok = conn.write_all(&out).is_ok();
            st = self.state.lock().unwrap();
            if !ok && st.session == session {
                st.broken = true;
                self.cv.notify_all();
            }
        }
    }

    /// Ends the session: every URB is cancelled and every interface given
    /// back, so that the next client starts from a clean device.
    fn end_session(&self) {
        {
            let mut st = self.state.lock().unwrap();
            st.session = 0;
            st.conn = None;
            st.replies.clear();
        }
        self.release_all(true);
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.dev.as_mut() {
            d.alt.clear();
        }
        st.broken = false;
    }

    // ------------------------------------------------------------------ URBs

    fn submit_urb(&self, session: u32, s: Submit, out: Vec<u8>) {
        if s.ep == 0 {
            let mut setup = s.setup;
            match usbip::classify_control(&setup) {
                Control::Forward => {}
                Control::SetConfiguration(value) => {
                    let current = self
                        .state
                        .lock()
                        .unwrap()
                        .dev
                        .as_ref()
                        .map(|d| d.info.configuration_value);
                    let code = if current == Some(value) {
                        status::OK
                    } else {
                        status::EPIPE
                    };
                    if code != status::OK {
                        warn!("usbip: SET_CONFIGURATION {value} refused (the device is in {current:?})");
                    }
                    self.reply(session, usbip::ret_submit(s.seqnum, code, 0, &[]));
                    return;
                }
                Control::SetInterface { interface, alt } => {
                    let code = self.set_interface(interface, alt, setup);
                    self.reply(session, usbip::ret_submit(s.seqnum, code, 0, &[]));
                    return;
                }
                Control::ClearHalt { endpoint } => {
                    if let Some(code) = self.clear_halt(endpoint) {
                        self.reply(session, usbip::ret_submit(s.seqnum, code, 0, &[]));
                        return;
                    }
                }
                Control::PortReset => {
                    self.port_reset();
                    self.reply(session, usbip::ret_submit(s.seqnum, status::OK, 0, &[]));
                    return;
                }
            }
            // A control IN's buffer may be longer than wLength; the transfer is wLength.
            let w_length = u16::from_le_bytes([setup[6], setup[7]]) as u32;
            if !s.dir_in && w_length != s.length {
                setup[6..8].copy_from_slice(&(s.length as u16).to_le_bytes());
            }
            if let Err(code) = self.submit(session, &s, &setup, &out, false) {
                self.reply(session, usbip::ret_submit(s.seqnum, code, 0, &[]));
            }
        } else if let Err(code) = self.submit(session, &s, &s.setup, &out, false) {
            self.reply(session, usbip::ret_submit(s.seqnum, code, 0, &[]));
        }
    }

    /// Submits one URB as a transfer; the reply comes from `transfer_cb`.
    fn submit(
        &self,
        session: u32,
        s: &Submit,
        setup: &[u8; 8],
        out: &[u8],
        park: bool,
    ) -> Result<(), i32> {
        let address = s.address();
        let mut st = self.state.lock().unwrap();
        let Some(dev) = st.dev.as_ref() else {
            return Err(status::ENODEV);
        };
        let handle = dev.handle;
        let (size, num_bytes) = if s.ep == 0 {
            let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
            if s.dir_in {
                let n = 8 + round_up(w_length, dev.mps0 as usize);
                (n, n)
            } else {
                (8 + out.len(), 8 + out.len())
            }
        } else {
            let found = usbdesc::interface_of(&dev.config, address, |i| dev.alt_of(i));
            let Some((interface, ep)) = found else {
                debug!("usbip: endpoint {address:02x} is in no selected interface");
                return Err(status::EINVAL);
            };
            if ep.attributes & 0x03 == 1 {
                return Err(status::EINVAL);
            }
            if !dev.claimed.contains_key(&interface) {
                let alt = dev.alt_of(interface);
                drop(st);
                let rc = unsafe {
                    usb::usb_host_interface_claim(self.client(), handle.0, interface, alt)
                };
                st = self.state.lock().unwrap();
                if rc != sys::ESP_OK {
                    warn!("usbip: interface {interface} could not be claimed (esp_err {rc}); the host has 8 pipes");
                    return Err(status::ENOMEM);
                }
                if let Some(d) = st.dev.as_mut() {
                    d.claimed.insert(interface, alt);
                }
            }
            if s.dir_in {
                let n = round_up(s.length as usize, ep.max_packet as usize);
                (n.max(1), n)
            } else {
                if ep.attributes & 0x03 == 2 && usbip::is_dap_packet_size_request(out) {
                    if let Some(d) = st.dev.as_mut() {
                        let vendor = usbdesc::interfaces(&d.config)
                            .iter()
                            .any(|i| i.number == interface && i.class == usbdesc::CLASS_VENDOR);
                        if vendor {
                            d.dap_info.insert(interface, ep.max_packet);
                        }
                    }
                }
                (out.len().max(1), out.len())
            }
        };
        // Wait for room; a lone URB larger than the limit goes alone.
        let (g, _) = self.wait_for(st, Duration::from_secs(30), |st| {
            st.session != session
                || st.inflight_bytes == 0
                || st.inflight_bytes + size <= MAX_INFLIGHT_BYTES
        });
        st = g;
        if st.session != session {
            return Err(status::ESHUTDOWN);
        }
        let mut t: *mut usb::usb_transfer_t = core::ptr::null_mut();
        let rc = unsafe { usb::usb_host_transfer_alloc(size, 0, &mut t) };
        if rc != sys::ESP_OK || t.is_null() {
            warn!("usbip: no memory for a {size}-byte transfer");
            return Err(status::ENOMEM);
        }
        unsafe {
            let tr = &mut *t;
            if s.ep == 0 {
                std::ptr::copy_nonoverlapping(setup.as_ptr(), tr.data_buffer, 8);
                if !s.dir_in {
                    std::ptr::copy_nonoverlapping(out.as_ptr(), tr.data_buffer.add(8), out.len());
                }
            } else if !s.dir_in {
                std::ptr::copy_nonoverlapping(out.as_ptr(), tr.data_buffer, out.len());
            }
            tr.num_bytes = num_bytes as i32;
            tr.device_handle = handle.0;
            tr.bEndpointAddress = address;
            tr.callback = Some(transfer_cb);
            tr.context = core::ptr::null_mut();
            tr.timeout_ms = 0;
            tr.flags = if s.flags & URB_ZERO_PACKET != 0 {
                usb::USB_TRANSFER_FLAG_ZERO_PACK
            } else {
                0
            };
        }
        st.inflight.insert(
            t as usize,
            Urb {
                session,
                seqnum: s.seqnum,
                ep: address,
                dir_in: s.dir_in,
                control: s.ep == 0,
                requested: s.length,
                bytes: size,
                unlink: None,
                park,
                internal: false,
            },
        );
        st.inflight_bytes += size;
        drop(st);
        let rc = unsafe {
            if s.ep == 0 {
                usb::usb_host_transfer_submit_control(self.client(), t)
            } else {
                usb::usb_host_transfer_submit(t)
            }
        };
        if rc != sys::ESP_OK {
            let mut st = self.state.lock().unwrap();
            if let Some(u) = st.inflight.remove(&(t as usize)) {
                st.inflight_bytes -= u.bytes;
            }
            drop(st);
            unsafe { usb::usb_host_transfer_free(t) };
            debug!("usbip: submit on {address:02x} failed: esp_err {rc}");
            return Err(if rc == sys::ESP_ERR_INVALID_STATE {
                status::EPIPE
            } else {
                status::EPROTO
            });
        }
        Ok(())
    }

    fn unlink(&self, session: u32, seqnum: u32, victim: u32) {
        let mut st = self.state.lock().unwrap();
        let found = st
            .inflight
            .iter_mut()
            .find(|(_, u)| u.session == session && u.seqnum == victim && !u.internal);
        let Some((_, urb)) = found else {
            // Already answered.
            st.replies.push_back(usbip::ret_unlink(seqnum, status::OK));
            self.cv.notify_all();
            return;
        };
        urb.unlink = Some(seqnum);
        if urb.control {
            // Endpoint 0 cannot be halted; the unlink is answered when the
            // transfer ends.
            return;
        }
        let ep = urb.ep;
        drop(st);
        self.cancel_endpoints(&[ep]);
        let handle = self.state.lock().unwrap().dev.as_ref().map(|d| d.handle);
        if let Some(h) = handle {
            unsafe { usb::usb_host_endpoint_clear(h.0, ep) };
        }
        self.resubmit_parked();
    }

    /// Halts and flushes endpoints; the transfers queued on them come back
    /// through `transfer_cb`, the ones that were not unlinked into `parked`.
    fn cancel_endpoints(&self, eps: &[u8]) {
        let mut st = self.state.lock().unwrap();
        let Some(handle) = st.dev.as_ref().map(|d| d.handle) else {
            return;
        };
        for u in st
            .inflight
            .values_mut()
            .filter(|u| eps.contains(&u.ep) && !u.internal)
        {
            u.park = true;
        }
        drop(st);
        for &ep in eps {
            unsafe {
                usb::usb_host_endpoint_halt(handle.0, ep);
                usb::usb_host_endpoint_flush(handle.0, ep);
            }
        }
        let st = self.state.lock().unwrap();
        let (st, done) = self.wait_for(st, SETTLE_TIMEOUT, |st| {
            !st.inflight
                .values()
                .any(|u| eps.contains(&u.ep) && !u.internal)
        });
        drop(st);
        if !done {
            warn!("usbip: transfers on {eps:02x?} did not come back after a flush");
        }
    }

    /// Submits the parked transfers again, oldest first.
    fn resubmit_parked(&self) {
        let mut st = self.state.lock().unwrap();
        let mut parked = std::mem::take(&mut st.parked);
        parked.sort_by_key(|(_, u)| u.seqnum);
        let handle = st.dev.as_ref().map(|d| d.handle);
        let session = st.session;
        for (key, mut urb) in parked {
            let t = key as *mut usb::usb_transfer_t;
            let endpoint_there = st.dev.as_ref().is_some_and(|d| {
                usbdesc::interface_of(&d.config, urb.ep, |i| d.alt_of(i)).is_some()
            });
            if urb.session != session || handle.is_none() || !endpoint_there {
                st.inflight_bytes -= urb.bytes;
                if urb.session == session {
                    st.replies
                        .push_back(usbip::ret_submit(urb.seqnum, status::ECONNRESET, 0, &[]));
                }
                unsafe { usb::usb_host_transfer_free(t) };
                continue;
            }
            urb.park = false;
            unsafe { (*t).actual_num_bytes = 0 };
            let seqnum = urb.seqnum;
            st.inflight.insert(key, urb);
            drop(st);
            let rc = unsafe { usb::usb_host_transfer_submit(t) };
            st = self.state.lock().unwrap();
            if rc != sys::ESP_OK {
                if let Some(u) = st.inflight.remove(&key) {
                    st.inflight_bytes -= u.bytes;
                }
                st.replies
                    .push_back(usbip::ret_submit(seqnum, status::EPIPE, 0, &[]));
                unsafe { usb::usb_host_transfer_free(t) };
            }
        }
        self.cv.notify_all();
    }

    /// Cancels everything and gives every interface back. With `drop_all`
    /// the cancelled transfers are not submitted again.
    fn release_all(&self, drop_all: bool) {
        let (handle, interfaces, eps) = {
            let st = self.state.lock().unwrap();
            let Some(d) = st.dev.as_ref() else { return };
            let interfaces: Vec<u8> = d.claimed.keys().copied().collect();
            let eps: Vec<u8> = interfaces.iter().flat_map(|&i| d.endpoints_of(i)).collect();
            (d.handle, interfaces, eps)
        };
        self.cancel_endpoints(&eps);
        if drop_all {
            let mut st = self.state.lock().unwrap();
            for (key, urb) in std::mem::take(&mut st.parked) {
                st.inflight_bytes -= urb.bytes;
                if urb.session == st.session && st.session != 0 {
                    st.replies
                        .push_back(usbip::ret_submit(urb.seqnum, status::ECONNRESET, 0, &[]));
                }
                unsafe { usb::usb_host_transfer_free(key as *mut usb::usb_transfer_t) };
            }
            // Control transfers cannot be cancelled: give them a moment.
            let (_st, _) = self.wait_for(st, SETTLE_TIMEOUT, |st| {
                !st.inflight.values().any(|u| !u.internal)
            });
            self.cv.notify_all();
        }
        for &interface in &interfaces {
            let rc = unsafe { usb::usb_host_interface_release(self.client(), handle.0, interface) };
            if rc != sys::ESP_OK {
                warn!("usbip: interface {interface} release failed: esp_err {rc}");
            }
        }
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.dev.as_mut() {
            d.claimed.clear();
        }
    }

    /// Gives back one interface after cancelling its transfers (parked for
    /// a later resubmit), so that its pipes are made again at DATA0.
    fn release_interface(&self, interface: u8) {
        let (handle, eps, claimed) = {
            let st = self.state.lock().unwrap();
            let Some(d) = st.dev.as_ref() else { return };
            (
                d.handle,
                d.endpoints_of(interface),
                d.claimed.contains_key(&interface),
            )
        };
        if !claimed {
            return;
        }
        self.cancel_endpoints(&eps);
        let rc = unsafe { usb::usb_host_interface_release(self.client(), handle.0, interface) };
        if rc != sys::ESP_OK {
            warn!("usbip: interface {interface} release failed: esp_err {rc}");
        }
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.dev.as_mut() {
            d.claimed.remove(&interface);
        }
    }

    /// A control transfer the server sends itself, waited for.
    fn control_sync(&self, setup: [u8; 8]) -> i32 {
        let Some(handle) = self.state.lock().unwrap().dev.as_ref().map(|d| d.handle) else {
            return status::ENODEV;
        };
        let mut t: *mut usb::usb_transfer_t = core::ptr::null_mut();
        if unsafe { usb::usb_host_transfer_alloc(8, 0, &mut t) } != sys::ESP_OK || t.is_null() {
            return status::ENOMEM;
        }
        unsafe {
            let tr = &mut *t;
            std::ptr::copy_nonoverlapping(setup.as_ptr(), tr.data_buffer, 8);
            tr.num_bytes = 8;
            tr.device_handle = handle.0;
            tr.bEndpointAddress = 0;
            tr.callback = Some(transfer_cb);
            tr.context = core::ptr::null_mut();
        }
        let key = t as usize;
        self.state.lock().unwrap().inflight.insert(
            key,
            Urb {
                session: 0,
                seqnum: 0,
                ep: 0,
                dir_in: false,
                control: true,
                requested: 0,
                bytes: 0,
                unlink: None,
                park: false,
                internal: true,
            },
        );
        if unsafe { usb::usb_host_transfer_submit_control(self.client(), t) } != sys::ESP_OK {
            self.state.lock().unwrap().inflight.remove(&key);
            unsafe { usb::usb_host_transfer_free(t) };
            return status::EPROTO;
        }
        let st = self.state.lock().unwrap();
        let (mut st, done) =
            self.wait_for(st, SETTLE_TIMEOUT, |st| st.internal_done.contains_key(&key));
        if !done {
            // Let the callback free it whenever it comes.
            if let Some(u) = st.inflight.get_mut(&key) {
                u.internal = false;
            }
            return status::ETIME;
        }
        let (code, actual) = st.internal_done.remove(&key).expect("checked");
        drop(st);
        unsafe { usb::usb_host_transfer_free(t) };
        // A control transfer's count includes its 8-byte setup packet.
        reply_status(code, actual.saturating_sub(8), 0).0
    }

    fn set_interface(&self, interface: u8, alt: u8, setup: [u8; 8]) -> i32 {
        self.release_interface(interface);
        let code = self.control_sync(setup);
        if code == status::OK {
            if let Some(d) = self.state.lock().unwrap().dev.as_mut() {
                d.alt.insert(interface, alt);
            }
        }
        // URBs of the old setting have nowhere to go now.
        self.resubmit_parked();
        code
    }

    /// CLEAR_FEATURE(ENDPOINT_HALT). The host library cannot set a pipe
    /// back to DATA0, so the endpoint's interface is released (its pipes
    /// are made again) and every endpoint of it is cleared on the device,
    /// which sets each to DATA0 there too. `None` when the endpoint is in
    /// no claimed interface: the request is then passed on as it is.
    fn clear_halt(&self, endpoint: u8) -> Option<i32> {
        let (interface, eps) = {
            let st = self.state.lock().unwrap();
            let d = st.dev.as_ref()?;
            let (interface, _) = usbdesc::interface_of(&d.config, endpoint, |i| d.alt_of(i))?;
            if !d.claimed.contains_key(&interface) {
                return None;
            }
            (interface, d.endpoints_of(interface))
        };
        self.release_interface(interface);
        let mut code = status::OK;
        for ep in eps {
            let c = self.control_sync([0x02, 0x01, 0, 0, ep, 0, 0, 0]);
            if ep == endpoint {
                code = c;
            }
        }
        self.resubmit_parked();
        Some(code)
    }

    /// The client reset the device. The board cannot reset its port for
    /// one client, so it brings the device back to how a reset leaves it as
    /// far as the client can tell: no transfers, interfaces in setting 0,
    /// every endpoint at DATA0.
    fn port_reset(&self) {
        let (interfaces, alts) = {
            let st = self.state.lock().unwrap();
            let Some(d) = st.dev.as_ref() else { return };
            let interfaces: Vec<u8> = usbdesc::interfaces(&d.config)
                .iter()
                .map(|i| i.number)
                .collect();
            let alts: Vec<u8> = d
                .alt
                .iter()
                .filter(|(_, &a)| a != 0)
                .map(|(&i, _)| i)
                .collect();
            (interfaces, alts)
        };
        self.release_all(true);
        for i in alts {
            self.control_sync([0x01, 0x0b, 0, 0, i, 0, 0, 0]);
        }
        let eps: Vec<u8> = {
            let mut st = self.state.lock().unwrap();
            let Some(d) = st.dev.as_mut() else { return };
            d.alt.clear();
            interfaces.iter().flat_map(|&i| d.endpoints_of(i)).collect()
        };
        for ep in eps {
            self.control_sync([0x02, 0x01, 0, 0, ep, 0, 0, 0]);
        }
        info!("usbip: the client reset the device");
    }
}

/// Fills `buf` from the connection. `timeout` bounds the whole read (`None`:
/// wait as long as `stop` says no).
fn read_exact(
    conn: &TcpStream,
    buf: &mut [u8],
    timeout: Option<Duration>,
    stop: impl Fn() -> bool,
) -> std::io::Result<()> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut filled = 0;
    while filled < buf.len() {
        match conn.read(&mut buf[filled..], Duration::from_millis(200)) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if stop() || deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
