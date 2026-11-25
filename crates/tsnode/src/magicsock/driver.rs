//! Drives [`MagicCore`] on `std`: a UDP socket read by one thread, a tick
//! thread, and one thread per DERP connection. Every input goes through the
//! core under one lock; what it puts out is sent before the lock is released,
//! except decrypted IP packets, which go to the stack after it (the stack
//! calls back into [`MagicSock::send_ip`]).

use super::{Event, MagicCore, PeerStatus, Transmit};
use crate::derp::{DerpClient, Event as DerpEvent};
use crate::keys::{short, KeyPair};
use crate::net::Dialer;
use crate::time::{Instant, WallClock};
use crate::types::{DerpMap, DerpNode, Node, PeerChange};
use alloc::collections::{BTreeMap, VecDeque};
use log::{debug, info, trace, warn};
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub type IpSink = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

const DERP_PING_IDLE: Duration = Duration::from_secs(55);
const DERP_PING_TIMEOUT: Duration = Duration::from_secs(15);
/// Packets queued for one DERP connection while it is (re)connecting.
const DERP_QUEUE_LIMIT: usize = if cfg!(feature = "small-ram") { 16 } else { 64 };
/// Receive buffer of the UDP socket: a datagram longer than this is cut.
const UDP_BUF: usize = if cfg!(feature = "small-ram") {
    2048
} else {
    65536
};
/// How often the LAN address is looked up again when not configured.
const LAN_IP_INTERVAL: Duration = Duration::from_secs(10);

struct DerpHandle {
    queue: Arc<DerpQueue>,
    connected: Arc<AtomicBool>,
}

impl Drop for DerpHandle {
    /// The DERP thread ends once its handle is gone.
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// Commands for one DERP thread, which polls them.
///
/// Not `std::sync::mpsc`: creating one of its channels puts a value aligned
/// to 64 bytes on the stack, and the Xtensa compiler realigns such a frame
/// by adding to the stack pointer without `movsp`. When the caller's
/// registers had already been spilled, its return address and stack pointer
/// were restored from the wrong place (the ESP32-S3 crashed about once in
/// 100 bridge connections while this was inlined into `flush`).
struct DerpQueue {
    state: Mutex<(VecDeque<DerpCmd>, bool)>,
}

impl DerpQueue {
    fn new() -> Self {
        DerpQueue {
            state: Mutex::new((VecDeque::new(), false)),
        }
    }

    /// False when the queue is full.
    fn push(&self, cmd: DerpCmd) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.0.len() >= DERP_QUEUE_LIMIT {
            return false;
        }
        st.0.push_back(cmd);
        true
    }

    /// The next command; `Close` once the handle is gone.
    fn pop(&self) -> Option<DerpCmd> {
        let mut st = self.state.lock().unwrap();
        match st.0.pop_front() {
            Some(c) => Some(c),
            None if st.1 => Some(DerpCmd::Close),
            None => None,
        }
    }

    fn close(&self) {
        self.state.lock().unwrap().1 = true;
    }
}

enum DerpCmd {
    Send { dst: [u8; 32], pkt: Vec<u8> },
    NotePreferred(bool),
    Close,
}

struct Inner {
    core: Mutex<MagicCore>,
    disco_public: [u8; 32],
    udp: UdpSocket,
    udp_port: u16,
    /// The LAN address given in the configuration; looked up when `None`.
    local_ip: Option<IpAddr>,
    derps: Mutex<BTreeMap<i32, DerpHandle>>,
    ip_sink: Mutex<Option<IpSink>>,
    dialer: Arc<dyn Dialer>,
    shutdown: AtomicBool,
    endpoints_changed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    stack_size: usize,
    spawner: crate::net::Spawner,
}

#[derive(Clone)]
pub struct MagicSock {
    inner: Arc<Inner>,
}

impl MagicSock {
    pub fn start(
        node_key: KeyPair,
        disco_key: KeyPair,
        dialer: Arc<dyn Dialer>,
        udp_port: u16,
        local_ip: Option<IpAddr>,
        stack_size: usize,
        spawner: crate::net::Spawner,
    ) -> io::Result<Self> {
        let udp = match UdpSocket::bind(("0.0.0.0", udp_port)) {
            Ok(s) => s,
            Err(e) => {
                warn!("magicsock: bind :{udp_port} failed ({e}); using an ephemeral port");
                UdpSocket::bind(("0.0.0.0", 0))?
            }
        };
        let bound_port = udp.local_addr()?.port();
        udp.set_read_timeout(Some(Duration::from_millis(500)))?;
        info!(
            "magicsock: node {} disco {} udp :{bound_port}",
            short(node_key.public()),
            short(disco_key.public())
        );
        let disco_public = *disco_key.public();
        let mut core = MagicCore::new(node_key, disco_key, bound_port, WallClock::system());
        core.set_lan_ip(local_ip.or_else(guess_local_ip));
        while core.poll_event().is_some() {}
        let inner = Arc::new(Inner {
            core: Mutex::new(core),
            disco_public,
            udp,
            udp_port: bound_port,
            local_ip,
            derps: Mutex::new(BTreeMap::new()),
            ip_sink: Mutex::new(None),
            dialer,
            shutdown: AtomicBool::new(false),
            endpoints_changed: Mutex::new(None),
            stack_size,
            spawner,
        });
        let ms = MagicSock { inner };
        let rx = ms.clone();
        (ms.inner.spawner)("magicsock-udp", stack_size, Box::new(move || rx.udp_loop()))?;
        let tick = ms.clone();
        (ms.inner.spawner)(
            "magicsock-tick",
            stack_size,
            Box::new(move || tick.tick_loop()),
        )?;
        Ok(ms)
    }

    fn core(&self) -> MutexGuard<'_, MagicCore> {
        self.inner.core.lock().unwrap()
    }

    /// Reads the core.
    pub fn inspect<R>(&self, f: impl FnOnce(&MagicCore) -> R) -> R {
        f(&self.core())
    }

    /// Runs `f` on the core, then carries out what it asked for.
    pub fn with_core<R>(&self, f: impl FnOnce(&mut MagicCore) -> R) -> R {
        let mut core = self.core();
        let r = f(&mut core);
        self.flush(core);
        r
    }

    /// Sends what the core put out and acts on its events. Decrypted IP
    /// packets and the endpoint callback are delivered after the lock is
    /// released.
    fn flush(&self, mut core: MutexGuard<'_, MagicCore>) {
        let mut ips = Vec::new();
        let mut endpoints_changed = false;
        while let Some(ev) = core.poll_event() {
            match ev {
                Event::Ip(p) => ips.push(p),
                Event::DerpOpen { region, node } => {
                    if !self.open_derp(region, node, core.node_key().clone()) {
                        core.derp_closed(region);
                    }
                }
                Event::DerpPreferred { region } => {
                    if let Some(h) = self.inner.derps.lock().unwrap().get(&region) {
                        h.queue.push(DerpCmd::NotePreferred(true));
                    }
                }
                Event::DerpClose { region } => {
                    // Dropping the handle closes the thread's queue.
                    self.inner.derps.lock().unwrap().remove(&region);
                }
                Event::EndpointsChanged => endpoints_changed = true,
            }
        }
        while let Some(t) = core.poll_transmit() {
            match t {
                Transmit::Udp { dst, data } => {
                    let _ = self.inner.udp.send_to(&data, dst);
                }
                Transmit::Derp { region, dst, data } => {
                    let derps = self.inner.derps.lock().unwrap();
                    let Some(h) = derps.get(&region) else {
                        continue;
                    };
                    if !h.queue.push(DerpCmd::Send { dst, pkt: data }) {
                        trace!("magicsock: DERP {region} send queue full; dropping packet");
                    }
                }
            }
        }
        drop(core);
        if !ips.is_empty() {
            let sink = self.inner.ip_sink.lock().unwrap().clone();
            if let Some(sink) = sink {
                for p in ips {
                    sink(p);
                }
            }
        }
        if endpoints_changed {
            let cb = self.inner.endpoints_changed.lock().unwrap().clone();
            if let Some(cb) = cb {
                cb();
            }
        }
    }

    pub fn set_ip_sink(&self, sink: IpSink) {
        *self.inner.ip_sink.lock().unwrap() = Some(sink);
    }

    pub fn set_endpoints_changed(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.endpoints_changed.lock().unwrap() = Some(cb);
    }

    pub fn node_public(&self) -> [u8; 32] {
        *self.core().node_key().public()
    }

    pub fn disco_public(&self) -> &[u8; 32] {
        &self.inner.disco_public
    }

    pub fn udp_port(&self) -> u16 {
        self.inner.udp_port
    }

    pub fn home_derp(&self) -> i32 {
        self.core().home_derp()
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        // Dropping the handles closes the threads' queues.
        let derps = std::mem::take(&mut *self.inner.derps.lock().unwrap());
        drop(derps);
    }

    pub fn local_endpoints(&self) -> Vec<SocketAddr> {
        self.core().local_endpoints()
    }

    pub fn derp_latency(&self) -> BTreeMap<String, f64> {
        self.core().derp_latency()
    }

    pub fn peer_status(&self) -> Vec<PeerStatus> {
        self.core().peer_status(Instant::now())
    }

    pub fn set_unix_time(&self, unix: Duration) {
        self.core().set_unix_time(Instant::now(), unix);
    }

    pub fn set_derp_map(&self, map: DerpMap) {
        self.core().set_derp_map(map);
    }

    pub fn set_peers(&self, nodes: Vec<Box<Node>>) {
        self.core().set_peers(nodes);
    }

    pub fn peers_changed(&self, nodes: Vec<Box<Node>>) {
        self.core().peers_changed(nodes);
    }

    pub fn peers_removed(&self, ids: &[i64]) {
        self.core().peers_removed(ids);
    }

    pub fn apply_patches(&self, patches: &[PeerChange]) {
        self.core().apply_patches(patches);
    }

    pub fn set_packet_filter(&self, filter: crate::filter::Filter) {
        self.core().set_packet_filter(filter);
    }

    pub fn set_node_key(&self, key: KeyPair) {
        self.with_core(|c| c.set_node_key(key));
    }

    /// Sends an IP packet produced by the local stack to the right peer.
    pub fn send_ip(&self, pkt: &[u8]) {
        self.with_core(|c| c.send_ip(pkt, Instant::now()));
    }

    /// Ensures a connection to the home DERP exists and is marked preferred.
    pub fn connect_home_derp(&self, region: i32) {
        self.with_core(|c| c.connect_home_derp(region, Instant::now()));
    }

    pub fn derp_connected(&self, region: i32) -> bool {
        self.inner
            .derps
            .lock()
            .unwrap()
            .get(&region)
            .map(|h| h.connected.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Sends STUN probes to all regions now and waits briefly for answers.
    /// Returns the region with the lowest latency, if any answered.
    pub fn probe_derp_regions(&self, wait: Duration) -> Option<i32> {
        self.with_core(|c| c.probe_derp_regions(Instant::now()));
        std::thread::sleep(wait);
        self.core().nearest_region()
    }

    /// Fallback home-region choice when STUN is blocked: prefer a region by
    /// code (e.g. "tok"), else the lowest region id.
    pub fn fallback_region(&self, preferred_code: &str) -> Option<i32> {
        self.core().fallback_region(preferred_code)
    }

    // ---- Threads ----

    /// Starts the thread for DERP `region`. False if it cannot be started.
    fn open_derp(&self, region: i32, node: DerpNode, key: KeyPair) -> bool {
        let queue = Arc::new(DerpQueue::new());
        let connected = Arc::new(AtomicBool::new(false));
        let me = self.clone();
        let q2 = queue.clone();
        let c2 = connected.clone();
        let name = format!("derp-{region}");
        if let Err(e) = (self.inner.spawner)(
            &name,
            self.inner.stack_size,
            Box::new(move || me.derp_loop(region, node, key, q2, c2)),
        ) {
            warn!("magicsock: cannot spawn DERP thread: {e}");
            return false;
        }
        self.inner
            .derps
            .lock()
            .unwrap()
            .insert(region, DerpHandle { queue, connected });
        true
    }

    fn derp_loop(
        &self,
        region: i32,
        node: DerpNode,
        key: KeyPair,
        queue: Arc<DerpQueue>,
        connected: Arc<AtomicBool>,
    ) {
        let mut backoff = Duration::from_secs(1);
        let mut preferred = false;
        'outer: while !self.inner.shutdown.load(Ordering::Relaxed) {
            let mut client =
                match DerpClient::connect(self.inner.dialer.as_ref(), &node, region, &key) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("derp[{region}]: connect to {} failed: {e}", node.host_name);
                        // Drain commands while waiting so the queue does not
                        // fill with stale packets.
                        let until = std::time::Instant::now() + backoff;
                        while std::time::Instant::now() < until {
                            while let Some(cmd) = queue.pop() {
                                match cmd {
                                    DerpCmd::Close => break 'outer,
                                    DerpCmd::NotePreferred(p) => preferred = p,
                                    DerpCmd::Send { .. } => {}
                                }
                            }
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        continue;
                    }
                };
            backoff = Duration::from_secs(1);
            connected.store(true, Ordering::Relaxed);
            info!("derp[{region}]: connected via {}", node.host_name);
            if preferred {
                let _ = client.note_preferred(true);
            }
            let mut pending_ping: Option<std::time::Instant> = None;
            loop {
                if self.inner.shutdown.load(Ordering::Relaxed) {
                    break 'outer;
                }
                match client.recv(Duration::from_millis(20)) {
                    Ok(Some(DerpEvent::Packet { src, data })) => {
                        self.with_core(|c| c.handle_derp(region, src, &data, Instant::now()))
                    }
                    Ok(Some(DerpEvent::PeerGone(k))) => {
                        trace!("derp[{region}]: peer gone {}", short(&k))
                    }
                    Ok(Some(DerpEvent::Pong(_))) => pending_ping = None,
                    Ok(Some(DerpEvent::Health(h))) => warn!("derp[{region}]: health: {h}"),
                    Ok(Some(DerpEvent::Restarting)) => {
                        info!("derp[{region}]: server restarting; reconnecting");
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("derp[{region}]: {e}; reconnecting");
                        break;
                    }
                }
                let mut failed = false;
                while let Some(cmd) = queue.pop() {
                    match cmd {
                        DerpCmd::Send { dst, pkt } => {
                            if let Err(e) = client.send_packet(&dst, &pkt) {
                                warn!("derp[{region}]: send failed: {e}");
                                failed = true;
                                break;
                            }
                        }
                        DerpCmd::NotePreferred(p) => {
                            preferred = p;
                            let _ = client.note_preferred(p);
                        }
                        DerpCmd::Close => break 'outer,
                    }
                }
                if failed {
                    break;
                }
                let now = std::time::Instant::now();
                if let Some(t) = pending_ping {
                    if now.duration_since(t) > DERP_PING_TIMEOUT {
                        warn!("derp[{region}]: ping timeout; reconnecting");
                        break;
                    }
                } else if now.duration_since(client.last_activity) > DERP_PING_IDLE {
                    if client.ping().is_err() {
                        break;
                    }
                    pending_ping = Some(now);
                }
            }
            connected.store(false, Ordering::Relaxed);
        }
        connected.store(false, Ordering::Relaxed);
        self.inner.derps.lock().unwrap().retain(|r, h| {
            !(*r == region && std::ptr::eq(h.connected.as_ref(), connected.as_ref()))
        });
        debug!("derp[{region}]: thread exit");
    }

    fn udp_loop(&self) {
        // Big enough for any datagram a peer sends (WireGuard packets are
        // under 1500 bytes; STUN and disco are small). 64 KB here was most
        // of an ESP32-C3's free RAM.
        let mut buf = vec![0u8; UDP_BUF];
        while !self.inner.shutdown.load(Ordering::Relaxed) {
            match self.inner.udp.recv_from(&mut buf) {
                Ok((n, from)) => self.with_core(|c| c.handle_udp(from, &buf[..n], Instant::now())),
                Err(e) if crate::net::is_timeout(&e) => {}
                Err(e) => {
                    warn!("magicsock: udp recv error: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn tick_loop(&self) {
        let mut last_lan_lookup = std::time::Instant::now();
        while !self.inner.shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            let lan =
                if self.inner.local_ip.is_none() && last_lan_lookup.elapsed() >= LAN_IP_INTERVAL {
                    last_lan_lookup = std::time::Instant::now();
                    Some(guess_local_ip())
                } else {
                    None
                };
            self.with_core(|c| {
                if let Some(ip) = lan {
                    c.set_lan_ip(ip);
                }
                c.tick(Instant::now());
            });
        }
    }
}

/// Best-effort LAN address: the source address the OS would use to reach a
/// public destination (no packets are sent).
pub fn guess_local_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_unspecified() || ip.is_loopback() {
        None
    } else {
        Some(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netstack::NetStack;

    struct NoDialer;
    impl Dialer for NoDialer {
        fn dial_tcp(&self, _: &str, _: u16) -> io::Result<Box<dyn crate::net::Conn>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no network in tests",
            ))
        }
        fn dial_tls(
            &self,
            _: &str,
            _: Option<IpAddr>,
            _: u16,
        ) -> io::Result<Box<dyn crate::net::Conn>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no network in tests",
            ))
        }
    }

    fn node_for(ms: &MagicSock, name: &str, ip: &str) -> Node {
        Node {
            id: 1,
            name: name.into(),
            key: Some(ms.node_public()),
            disco_key: Some(*ms.disco_public()),
            addresses: vec![(ip.parse().unwrap(), 32)],
            allowed_ips: vec![(ip.parse().unwrap(), 32)],
            endpoints: vec![format!("127.0.0.1:{}", ms.udp_port()).parse().unwrap()],
            ..Default::default()
        }
    }

    /// Two nodes on localhost: TCP through WireGuard through UDP, direct path
    /// discovered with disco pings.
    #[test]
    fn tcp_over_wireguard_between_two_nodes() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dialer: Arc<dyn Dialer> = Arc::new(NoDialer);
        let start = || {
            MagicSock::start(
                KeyPair::generate(),
                KeyPair::generate(),
                dialer.clone(),
                0,
                Some("127.0.0.1".parse().unwrap()),
                256 * 1024,
                crate::net::std_spawner(),
            )
            .unwrap()
        };
        let (a, b) = (start(), start());
        let (a2, b2) = (a.clone(), b.clone());
        let ns_a = NetStack::start(
            Box::new(move |p| a2.send_ip(&p)),
            256 * 1024,
            &crate::net::std_spawner(),
        )
        .unwrap();
        let ns_b = NetStack::start(
            Box::new(move |p| b2.send_ip(&p)),
            256 * 1024,
            &crate::net::std_spawner(),
        )
        .unwrap();
        let (sa, sb) = (ns_a.clone(), ns_b.clone());
        a.set_ip_sink(Arc::new(move |p| sa.inject(p)));
        b.set_ip_sink(Arc::new(move |p| sb.inject(p)));
        a.set_packet_filter(crate::filter::Filter::allow_all());
        b.set_packet_filter(crate::filter::Filter::allow_all());
        ns_a.set_addresses(vec![("100.64.0.1".parse().unwrap(), 10)]);
        ns_b.set_addresses(vec![("100.64.0.2".parse().unwrap(), 10)]);
        a.set_peers(vec![Box::new(node_for(&b, "b", "100.64.0.2"))]);
        b.set_peers(vec![Box::new(node_for(&a, "a", "100.64.0.1"))]);

        let listener = ns_b.listen(2300);
        let client = ns_a.connect("100.64.0.2:2300".parse().unwrap());
        let server = listener
            .accept(Duration::from_secs(10))
            .expect("accept through wireguard");
        client.write_all(b"ping through wg").unwrap();
        let mut buf = [0u8; 128];
        let n = server.read(&mut buf, Duration::from_secs(10)).unwrap();
        assert_eq!(&buf[..n], b"ping through wg");
        server.write_all(b"pong").unwrap();
        let n = client.read(&mut buf, Duration::from_secs(10)).unwrap();
        assert_eq!(&buf[..n], b"pong");

        // Discovery should have established a disco-verified direct path.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let st = a.peer_status();
            if st[0].direct.is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no direct path discovered: {st:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        a.shutdown();
        b.shutdown();
        ns_a.shutdown();
        ns_b.shutdown();
    }
}
