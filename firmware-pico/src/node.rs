//! The Tailscale node on embassy: tsnode's state machines ([`NodeCore`],
//! [`MagicCore`], [`NetCore`]) driven by tasks on core 0.
//!
//! - `control_task` talks to the control server: registration, then the
//!   map stream, whose messages it hands to the node core.
//! - `udp_task` reads the WireGuard and disco socket.
//! - `derp_task` (one per DERP connection) keeps a DERP server's TLS
//!   connection.
//! - `pump_task` carries out what the cores put out: datagrams to send,
//!   packets between WireGuard and the TCP/IP stack, DERP connections to
//!   open, timers.
//!
//! Every task runs on core 0's executor, so the cores are shared through a
//! `RefCell` that is never borrowed across an `await`. Core 1 (the shell)
//! sees only [`snapshot`], refreshed every second.

use crate::{clock, config, control, shell};
use adapter_core::shell::NodeState as ShellNodeState;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::net::{IpAddr, SocketAddr};
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant as EInstant, Timer};
use log::{info, warn};
use portable_atomic::{AtomicBool, Ordering};
use static_cell::StaticCell;
use tsnode::derp::{DerpSession, Event as DerpEvent};
use tsnode::keys::KeyPair;
use tsnode::magicsock::{Event, MagicCore, Transmit};
use tsnode::netstack::NetCore;
use tsnode::node::{
    Action, NodeCore, NodeSettings, NodeState, Registration, KEY_DISCO, KEY_HOME_DERP, KEY_MACHINE,
    KEY_NODE, KEY_REGISTERED,
};
use tsnode::time::{Instant, WallClock};
use tsnode::types::{DerpNode, MapResponse, RegisterResponse};

const UDP_PORT: u16 = 41641;
/// DERP connections at once: the home region and peers' homes.
const DERP_SLOTS: usize = 3;
const DERP_QUEUE: usize = 16;
const DERP_PING_IDLE: Duration = Duration::from_secs(55);
const DERP_PING_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one wait for a map message lasts: the node's timers run between.
const MAP_POLL: Duration = Duration::from_secs(1);
/// A map message longer than this is refused (the first one of a tailnet of
/// 49 peers is 69 KB).
const MAP_MESSAGE_MAX: usize = 256 * 1024;

/// tsnode's clock: microseconds since boot.
pub fn now() -> Instant {
    Instant::from_micros(EInstant::now().as_micros())
}

pub struct Cores {
    pub node: NodeCore,
    pub magic: MagicCore,
    pub net: NetCore,
}

/// Something only core 0's tasks touch.
struct CoreLocal<T>(T);
// SAFETY: the value is reached only from tasks of core 0's executor (one
// thread, cooperative), never from core 1 or an interrupt.
unsafe impl<T> Sync for CoreLocal<T> {}
unsafe impl<T> Send for CoreLocal<T> {}

pub struct Node {
    cores: RefCell<Cores>,
    udp: UdpSocket<'static>,
    /// The DERP region each connection slot serves.
    slots: RefCell<[Option<i32>; DERP_SLOTS]>,
    stack: Stack<'static>,
    machine_key: KeyPair,
}

enum DerpCmd {
    Send { dst: [u8; 32], data: Vec<u8> },
    Preferred,
    Close,
}

static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DERP_START: [Signal<CriticalSectionRawMutex, (i32, DerpNode)>; DERP_SLOTS] =
    [const { Signal::new() }; DERP_SLOTS];
static DERP_CMDS: [Channel<CriticalSectionRawMutex, DerpCmd, DERP_QUEUE>; DERP_SLOTS] =
    [const { Channel::new() }; DERP_SLOTS];
static DERP_UP: [AtomicBool; DERP_SLOTS] = [const { AtomicBool::new(false) }; DERP_SLOTS];
static STARTED: AtomicBool = AtomicBool::new(false);

/// What the shell shows of the node.
#[derive(Clone)]
pub struct Snapshot {
    pub state: ShellNodeState,
    pub derp: String,
    pub peers: String,
}

static SNAPSHOT: critical_section::Mutex<RefCell<Option<Snapshot>>> =
    critical_section::Mutex::new(RefCell::new(None));

/// The node as last seen; `None` before it started.
pub fn snapshot() -> Option<Snapshot> {
    critical_section::with(|cs| SNAPSHOT.borrow_ref(cs).clone())
}

fn load_or_create_key(name: &str) -> KeyPair {
    if let Some(k) = config::values()
        .get(name)
        .and_then(|v| KeyPair::from_secret_hex(v))
    {
        return k;
    }
    let k = KeyPair::generate();
    if let Err(e) = config::modify(|v| {
        v.insert(name.into(), k.secret_hex());
    }) {
        warn!("node: cannot store {name}: {e}");
    }
    k
}

fn ipv4_endpoint(a: SocketAddr) -> Option<IpEndpoint> {
    match a {
        SocketAddr::V4(v4) => Some(IpEndpoint::new(IpAddress::Ipv4(*v4.ip()), v4.port())),
        SocketAddr::V6(_) => None,
    }
}

/// Starts the node, once. Needs Wi-Fi up and the settings saved.
pub fn start(spawner: Spawner, stack: Stack<'static>) -> Result<(), String> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let cfg = shell::saved_config().ok_or("not configured")?;
    let machine_key = load_or_create_key(KEY_MACHINE);
    let node_key = load_or_create_key(KEY_NODE);
    let disco_key = load_or_create_key(KEY_DISCO);
    info!(
        "node: machine {} node {}",
        tsnode::keys::short(machine_key.public()),
        tsnode::keys::short(node_key.public())
    );
    let values = config::values();
    let t = now();

    static UDP_RX_META: StaticCell<CoreLocal<[PacketMetadata; 16]>> = StaticCell::new();
    static UDP_TX_META: StaticCell<CoreLocal<[PacketMetadata; 16]>> = StaticCell::new();
    static UDP_RX: StaticCell<CoreLocal<[u8; 8192]>> = StaticCell::new();
    static UDP_TX: StaticCell<CoreLocal<[u8; 8192]>> = StaticCell::new();
    let mut udp = UdpSocket::new(
        stack,
        &mut UDP_RX_META.init(CoreLocal([PacketMetadata::EMPTY; 16])).0,
        &mut UDP_RX.init(CoreLocal([0; 8192])).0,
        &mut UDP_TX_META.init(CoreLocal([PacketMetadata::EMPTY; 16])).0,
        &mut UDP_TX.init(CoreLocal([0; 8192])).0,
    );
    udp.bind(UDP_PORT).map_err(|e| format!("udp bind: {e:?}"))?;

    let wall = WallClock::new(t, core::time::Duration::from_secs(clock::now()));
    let mut magic = MagicCore::new(node_key.clone(), disco_key, UDP_PORT, wall);
    magic.set_lan_ip(stack.config_v4().map(|c| IpAddr::V4(c.address.address())));
    while magic.poll_event().is_some() {}
    let mut seed = [0u8; 8];
    let _ = getrandom::getrandom(&mut seed);
    let net = NetCore::new(u64::from_le_bytes(seed), t);
    let node = NodeCore::new(
        NodeSettings {
            hostname: cfg.hostname.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            auth_key: cfg.auth_key.clone(),
            fallback_region_code: "tok".into(),
        },
        node_key,
        values.get(KEY_REGISTERED).map(String::as_str) == Some("1"),
        values.get(KEY_HOME_DERP).and_then(|v| v.parse().ok()),
    );

    static NODE: StaticCell<CoreLocal<Node>> = StaticCell::new();
    let n: &'static Node = &NODE
        .init(CoreLocal(Node {
            cores: RefCell::new(Cores { node, magic, net }),
            udp,
            slots: RefCell::new([None; DERP_SLOTS]),
            stack,
            machine_key,
        }))
        .0;
    *NODE_REF.0.borrow_mut() = Some(n);
    spawner.spawn(pump_task(n).map_err(|_| "cannot start the node")?);
    spawner.spawn(udp_task(n).map_err(|_| "cannot start the node")?);
    for slot in 0..DERP_SLOTS {
        spawner.spawn(derp_task(n, slot).map_err(|_| "cannot start the node")?);
    }
    spawner.spawn(control_task(n).map_err(|_| "cannot start the node")?);
    Ok(())
}

impl Node {
    /// Runs `f` on the cores, carries out the node's actions and wakes the
    /// pump to send what they put out.
    fn with<R>(&self, f: impl FnOnce(&mut Cores, Instant) -> R) -> R {
        let (r, actions) = {
            let mut c = self.cores.borrow_mut();
            let r = f(&mut c, now());
            let actions: Vec<Action> = core::iter::from_fn(|| c.node.poll_action()).collect();
            for a in &actions {
                if let Action::Addresses(cidrs) = a {
                    c.net.set_addresses(cidrs);
                }
            }
            (r, actions)
        };
        for a in actions {
            if let Action::Store { key, value } = a {
                let r = config::modify(|v| match value {
                    Some(val) => {
                        v.insert(key.into(), val);
                    }
                    None => {
                        v.remove(key);
                    }
                });
                if let Err(e) = r {
                    warn!("node: cannot store {key}: {e}");
                }
            }
        }
        WAKE.signal(());
        r
    }

    fn open_derp(&self, region: i32, node: DerpNode) -> bool {
        let mut slots = self.slots.borrow_mut();
        if slots.contains(&Some(region)) {
            return true;
        }
        let Some(i) = slots.iter().position(Option::is_none) else {
            warn!("node: no free DERP connection for region {region}");
            return false;
        };
        slots[i] = Some(region);
        DERP_START[i].signal((region, node));
        true
    }

    fn slot(&self, region: i32) -> Option<usize> {
        self.slots.borrow().iter().position(|s| *s == Some(region))
    }

    fn derp_cmd(&self, region: i32, cmd: DerpCmd) {
        if let Some(i) = self.slot(region) {
            // A full queue drops the packet, as a congested link would.
            let _ = DERP_CMDS[i].try_send(cmd);
        }
    }

    fn derp_connected(&self, region: i32) -> bool {
        self.slot(region)
            .is_some_and(|i| DERP_UP[i].load(Ordering::Relaxed))
    }

    /// Refreshes what the shell sees.
    fn publish(&self) {
        let st = {
            let c = self.cores.borrow();
            c.node.status(&c.magic, now())
        };
        let state = match &st.state {
            NodeState::Starting | NodeState::ConnectingControl | NodeState::Registering => {
                ShellNodeState::Connecting
            }
            NodeState::NeedsLogin(url) => ShellNodeState::NeedsLogin(url.clone()),
            NodeState::Running => ShellNodeState::Running {
                addresses: st.addresses.iter().map(|a| a.to_string()).collect(),
                dns_name: st.dns_name.clone(),
            },
            NodeState::Error(e) => ShellNodeState::Error(e.clone()),
        };
        let derp = if st.home_derp == 0 {
            "none".to_string()
        } else {
            format!(
                "region {}{}",
                st.home_derp,
                if self.derp_connected(st.home_derp) {
                    " (connected)"
                } else {
                    " (down)"
                }
            )
        };
        let active: Vec<String> = st
            .peers
            .iter()
            .filter(|p| p.active || p.has_session)
            .map(|p| {
                format!(
                    "{}{}",
                    p.name,
                    p.direct
                        .map(|d| format!("@{d}"))
                        .unwrap_or_else(|| "@derp".into())
                )
            })
            .collect();
        let peers = format!(
            "{} known, active: {}",
            st.peers.len(),
            if active.is_empty() {
                "-".into()
            } else {
                active.join(", ")
            }
        );
        critical_section::with(|cs| {
            *SNAPSHOT.borrow_ref_mut(cs) = Some(Snapshot { state, derp, peers })
        });
    }
}

/// Carries out what the cores put out, and runs their timers.
#[embassy_executor::task]
async fn pump_task(n: &'static Node) -> ! {
    let mut next_tick = now();
    loop {
        let mut udp_out: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        let mut publish = false;
        // Packets went through the TCP/IP stack: the bridge has news.
        let mut traffic = false;
        let delay = {
            let mut c = n.cores.borrow_mut();
            let Cores { node, magic, net } = &mut *c;
            let t = now();
            if t >= next_tick {
                magic.tick(t);
                next_tick = t + core::time::Duration::from_secs(1);
                publish = true;
            }
            loop {
                let mut busy = false;
                while let Some(ev) = magic.poll_event() {
                    busy = true;
                    match ev {
                        Event::Ip(p) => {
                            traffic = true;
                            net.receive(p);
                        }
                        Event::DerpOpen { region, node: dn } => {
                            if !n.open_derp(region, dn) {
                                magic.derp_closed(region);
                            }
                        }
                        Event::DerpPreferred { region } => n.derp_cmd(region, DerpCmd::Preferred),
                        Event::DerpClose { region } => n.derp_cmd(region, DerpCmd::Close),
                        Event::EndpointsChanged => node.endpoints_changed(),
                    }
                }
                net.poll(t);
                while let Some(p) = net.poll_transmit() {
                    busy = true;
                    traffic = true;
                    magic.send_ip(&p, t);
                }
                while let Some(tx) = magic.poll_transmit() {
                    busy = true;
                    match tx {
                        Transmit::Udp { dst, data } => udp_out.push((dst, data)),
                        Transmit::Derp { region, dst, data } => {
                            n.derp_cmd(region, DerpCmd::Send { dst, data })
                        }
                    }
                }
                if !busy {
                    break;
                }
            }
            let until_tick = next_tick.duration_since(t);
            net.poll_delay(t).map_or(until_tick, |d| d.min(until_tick))
        };
        if traffic {
            crate::bridge::wake();
        }
        for (dst, data) in udp_out {
            if let Some(ep) = ipv4_endpoint(dst) {
                let _ = n.udp.send_to(&data, ep).await;
            }
        }
        if publish {
            n.publish();
        }
        let delay = Duration::from_micros(delay.as_micros() as u64);
        select(WAKE.wait(), Timer::after(delay)).await;
    }
}

#[embassy_executor::task]
async fn udp_task(n: &'static Node) -> ! {
    let mut buf = vec![0u8; 2048];
    loop {
        match n.udp.recv_from(&mut buf).await {
            Ok((len, meta)) => {
                let from = SocketAddr::new(meta.endpoint.addr.into(), meta.endpoint.port);
                n.with(|c, t| c.magic.handle_udp(from, &buf[..len], t));
            }
            Err(e) => {
                warn!("node: udp: {e:?}");
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One DERP connection slot: waits to be given a region, then keeps a
/// connection to it until told to close.
#[embassy_executor::task(pool_size = DERP_SLOTS)]
async fn derp_task(n: &'static Node, slot: usize) -> ! {
    loop {
        let (region, dn) = DERP_START[slot].wait().await;
        derp_region(n, slot, region, &dn).await;
        DERP_UP[slot].store(false, Ordering::Relaxed);
        n.slots.borrow_mut()[slot] = None;
        while DERP_CMDS[slot].try_receive().is_ok() {}
        info!("derp[{region}]: closed");
    }
}

/// Keeps a connection to `region` until a `Close` comes.
async fn derp_region(n: &'static Node, slot: usize, region: i32, dn: &DerpNode) {
    let mut backoff = Duration::from_secs(1);
    let mut preferred = false;
    loop {
        match derp_session(n, slot, region, dn, &mut preferred).await {
            Ok(()) => return,
            Err(e) => warn!("derp[{region}]: {e}"),
        }
        DERP_UP[slot].store(false, Ordering::Relaxed);
        // Commands keep coming while we wait; only a Close matters.
        let until = EInstant::now() + backoff;
        loop {
            match select(DERP_CMDS[slot].receive(), Timer::at(until)).await {
                Either::First(DerpCmd::Close) => return,
                Either::First(DerpCmd::Preferred) => preferred = true,
                Either::First(_) => {}
                Either::Second(()) => break,
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One connection to a DERP server; `Ok` when told to close.
async fn derp_session(
    n: &'static Node,
    slot: usize,
    region: i32,
    dn: &DerpNode,
    preferred: &mut bool,
) -> Result<(), String> {
    let addr = match dn.ipv4 {
        Some(a) => IpAddress::Ipv4(a),
        None => control::resolve(n.stack, &dn.host_name).await?,
    };
    let port = if dn.derp_port > 0 {
        dn.derp_port as u16
    } else {
        443
    };
    let mut rx = vec![0u8; 4096];
    let mut tx = vec![0u8; 4096];
    let mut sock = embassy_net::tcp::TcpSocket::new(n.stack, &mut rx, &mut tx);
    sock.set_timeout(Some(Duration::from_secs(120)));
    sock.connect((addr, port))
        .await
        .map_err(|e| format!("connect {}: {e:?}", dn.host_name))?;
    let mut tls = crate::tls::TlsStream::connect(sock, &dn.host_name).await?;
    let key = n.cores.borrow().magic.node_key().clone();
    let mut session = DerpSession::new(&dn.host_name, &key);
    let deadline = EInstant::now() + Duration::from_secs(15);
    while !session.is_ready() {
        if session.has_output() {
            tls.write_all(&session.take_output()).await?;
        }
        if EInstant::now() >= deadline {
            return Err("handshake timed out".into());
        }
        let want = session.wanted(4096);
        let got = tls.read(session.read_buffer(want)).await;
        match got {
            Ok(0) => {
                let _ = session.received(0);
                return Err("closed in the handshake".into());
            }
            Ok(k) => session.received(k).map_err(|e| e.to_string())?,
            Err(e) => {
                let _ = session.received(0);
                return Err(e);
            }
        }
    }
    DERP_UP[slot].store(true, Ordering::Relaxed);
    info!("derp[{region}]: connected via {}", dn.host_name);
    if *preferred {
        session.note_preferred(true);
    }
    let mut last_activity = EInstant::now();
    let mut ping_sent: Option<EInstant> = None;
    loop {
        // Hand over every whole frame that came.
        loop {
            let frames = session.frames_received;
            let ev = session.poll_event().map_err(|e| e.to_string())?;
            if session.frames_received != frames {
                last_activity = EInstant::now();
            }
            match ev {
                Some(DerpEvent::Packet { src, data }) => {
                    n.with(|c, t| c.magic.handle_derp(region, src, &data, t))
                }
                Some(DerpEvent::Pong(_)) => ping_sent = None,
                Some(DerpEvent::Health(h)) => warn!("derp[{region}]: health: {h}"),
                Some(DerpEvent::Restarting) => return Err("server restarting".into()),
                Some(DerpEvent::PeerGone(_)) => {}
                None if session.frames_received != frames => {}
                None => break,
            }
        }
        if session.has_output() {
            tls.write_all(&session.take_output()).await?;
        }
        let timer = match ping_sent {
            Some(t) => t + DERP_PING_TIMEOUT,
            None => last_activity + DERP_PING_IDLE,
        };
        match select3(tls.readable(), DERP_CMDS[slot].receive(), Timer::at(timer)).await {
            Either3::First(()) => {
                let want = session.wanted(4096);
                match tls.read(session.read_buffer(want)).await {
                    Ok(0) => {
                        let _ = session.received(0);
                        return Err("connection closed".into());
                    }
                    Ok(k) => session.received(k).map_err(|e| e.to_string())?,
                    Err(e) => {
                        let _ = session.received(0);
                        return Err(e);
                    }
                }
            }
            Either3::Second(DerpCmd::Send { dst, data }) => {
                session
                    .send_packet(&dst, &data)
                    .map_err(|e| e.to_string())?;
                // Send what else is queued in the same write.
                while let Ok(cmd) = DERP_CMDS[slot].try_receive() {
                    match cmd {
                        DerpCmd::Send { dst, data } => session
                            .send_packet(&dst, &data)
                            .map_err(|e| e.to_string())?,
                        DerpCmd::Preferred => {
                            *preferred = true;
                            session.note_preferred(true)
                        }
                        DerpCmd::Close => {
                            tls.close();
                            return Ok(());
                        }
                    }
                }
            }
            Either3::Second(DerpCmd::Preferred) => {
                *preferred = true;
                session.note_preferred(true);
            }
            Either3::Second(DerpCmd::Close) => {
                tls.close();
                return Ok(());
            }
            Either3::Third(()) => {
                if ping_sent.is_some() {
                    return Err("ping timeout".into());
                }
                session.ping(tsnode::crypto::random_array());
                ping_sent = Some(EInstant::now());
            }
        }
    }
}

/// Talks to the control server for as long as the node runs.
#[embassy_executor::task]
async fn control_task(n: &'static Node) -> ! {
    let mut backoff = Duration::from_secs(2);
    loop {
        n.with(|c, _| c.node.connecting());
        n.publish();
        match control_session(n, &mut backoff).await {
            Ok(()) => {}
            Err(e) => {
                warn!("node: {e}");
                if n.with(|c, _| c.node.map_ended(&e)) {
                    backoff = Duration::from_secs(2);
                }
            }
        }
        n.publish();
        Timer::after(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One round: the server key, registration if needed, then the map stream
/// until it ends.
async fn control_session(n: &'static Node, backoff: &mut Duration) -> Result<(), String> {
    let stack = n.stack;
    let addr = control::resolve(stack, control::HOST).await?;
    control::sync_clock(stack, addr).await?;
    n.with(|c, t| {
        c.magic
            .set_unix_time(t, core::time::Duration::from_secs(clock::now()))
    });
    let server_key = control::fetch_key(stack, addr).await?;

    if n.cores.borrow().node.needs_registration() {
        loop {
            let req = n.with(|c, _| c.node.register_request());
            let json = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
            let mut rx = vec![0u8; 8192];
            let mut tx = vec![0u8; 4096];
            let mut io = control::ControlIo::open(
                stack,
                &mut rx,
                &mut tx,
                addr,
                n.machine_key.secret(),
                &server_key,
            )
            .await?;
            let (status, body) = io.post_json("/machine/register", &json, 64 * 1024).await?;
            io.close();
            if status != 200 {
                return Err(format!("register: HTTP {status}"));
            }
            let resp: RegisterResponse =
                serde_json::from_slice(&body).map_err(|e| format!("register: bad json: {e}"))?;
            match n.with(|c, _| c.node.on_register_response(resp, &mut c.magic)) {
                Ok(Registration::Again) => continue,
                Ok(Registration::Authorized) => {
                    forget_auth_key();
                    break;
                }
                Ok(Registration::Waiting) => {
                    n.publish();
                    // Someone has to approve the node; ask again later.
                    *backoff = Duration::from_secs(10);
                    return Ok(());
                }
                Err(e) => return Err(format!("register: {e}")),
            }
        }
    }

    let req = n.with(|c, t| {
        c.node.starting_map(&mut c.magic, t);
        c.node.map_request(true, &c.magic)
    });
    let json = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    let mut rx = vec![0u8; 8192];
    let mut tx = vec![0u8; 4096];
    let mut io = control::ControlIo::open(
        stack,
        &mut rx,
        &mut tx,
        addr,
        n.machine_key.secret(),
        &server_key,
    )
    .await?;
    let (id, status) = io.start_post("/machine/map", &json).await?;
    if status != 200 {
        return Err(format!("map: HTTP {status}"));
    }
    n.with(|c, t| c.node.map_connected(t));
    *backoff = Duration::from_secs(2);
    n.publish();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match io.next_chunk(id, MAP_POLL).await? {
            control::Chunk::Data(d) => append_map_data(&mut buf, &d),
            control::Chunk::End => return Err("map stream closed".into()),
            control::Chunk::Idle => {}
        }
        while buf.len() >= 4 {
            let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if len > MAP_MESSAGE_MAX {
                return Err(format!("map message too large ({len} bytes)"));
            }
            if buf.len() < 4 + len {
                break;
            }
            let parsed = serde_json::from_slice::<MapResponse>(&buf[4..4 + len]);
            buf.drain(..4 + len);
            // Give the message's block back before the next one.
            buf.shrink_to_fit();
            if buf.len() >= 4 {
                reserve_map_message(&mut buf);
            }
            match parsed {
                Ok(msg) => n.with(|c, t| c.node.on_map_response(msg, &mut c.magic, t)),
                Err(e) => warn!("node: map: bad json: {e}"),
            }
        }
        let (silent, lite) = n.with(|c, t| {
            c.node.tick(&mut c.magic, t);
            (c.node.map_silent(t), c.node.lite_update(&c.magic, t))
        });
        if silent {
            return Err("no keep-alive from control".into());
        }
        if let Some(mut lite) = lite {
            lite.stream = false;
            if let Err(e) = map_once(n, addr, &server_key, &lite).await {
                warn!("node: endpoint update failed: {e}");
                n.with(|c, _| c.node.lite_update_failed());
            }
        }
    }
}

/// Adds stream bytes to `buf`. Once a message's length prefix is in, the
/// buffer is sized for the whole message: grown by doubling, a 70 KB map
/// took a 128 KB block, and the heap had no such block free when the stream
/// reconnected.
fn append_map_data(buf: &mut Vec<u8>, mut d: &[u8]) {
    if buf.len() < 4 {
        let k = (4 - buf.len()).min(d.len());
        buf.extend_from_slice(&d[..k]);
        d = &d[k..];
        if buf.len() == 4 {
            reserve_map_message(buf);
        }
    }
    buf.extend_from_slice(d);
}

/// Sizes `buf`, which starts with a length prefix, for the whole message.
fn reserve_map_message(buf: &mut Vec<u8>) {
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // An oversized length is refused by the caller.
    if len <= MAP_MESSAGE_MAX {
        buf.reserve_exact((4 + len).saturating_sub(buf.len()));
    }
}

/// A single map request (an endpoint update) on a connection of its own.
async fn map_once(
    n: &'static Node,
    addr: IpAddress,
    server_key: &[u8; 32],
    req: &tsnode::types::MapRequest,
) -> Result<(), String> {
    let json = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut rx = vec![0u8; 4096];
    let mut tx = vec![0u8; 4096];
    let mut io = control::ControlIo::open(
        n.stack,
        &mut rx,
        &mut tx,
        addr,
        n.machine_key.secret(),
        server_key,
    )
    .await?;
    let (status, _) = io.post_json("/machine/map", &json, 64 * 1024).await?;
    io.close();
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    Ok(())
}

/// The auth key is needed only to register; drop it from flash after.
fn forget_auth_key() {
    if config::values().contains_key("authkey") {
        match config::modify(|v| {
            v.remove("authkey");
        }) {
            Ok(()) => info!("node: auth key removed from flash after registration"),
            Err(e) => warn!("node: cannot remove the auth key: {e}"),
        }
    }
}

/// The node's stack, for the bridge.
pub fn with_net<R>(f: impl FnOnce(&mut NetCore) -> R) -> Option<R> {
    let n = NODE_REF.get()?;
    let r = f(&mut n.cores.borrow_mut().net);
    WAKE.signal(());
    Some(r)
}

/// Set once the node runs, for [`with_net`].
static NODE_REF: OnceNode = OnceNode(RefCell::new(None));
struct OnceNode(RefCell<Option<&'static Node>>);
// SAFETY: as for `CoreLocal`: only core 0's tasks reach it.
unsafe impl Sync for OnceNode {}
impl OnceNode {
    fn get(&self) -> Option<&'static Node> {
        *self.0.borrow()
    }
}
