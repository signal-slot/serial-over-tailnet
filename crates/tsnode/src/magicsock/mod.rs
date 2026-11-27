//! "magicsock": moves WireGuard and disco packets between peers over UDP
//! (direct) or DERP (relayed), chooses paths, and drives STUN endpoint
//! discovery.
//!
//! [`MagicCore`] is the state machine and does no I/O. The caller hands it
//! what arrived ([`MagicCore::handle_udp`], [`MagicCore::handle_derp`]), what
//! the local stack sends ([`MagicCore::send_ip`]) and the passing of time
//! ([`MagicCore::tick`]), then takes out the datagrams to send
//! ([`MagicCore::poll_transmit`]) and what else happened
//! ([`MagicCore::poll_event`]: decrypted IP packets, DERP connections to open
//! or close, a new public endpoint).
//!
//! `MagicSock` (`std`) drives it with a UDP socket and one thread per DERP
//! connection.

#[cfg(feature = "std")]
mod driver;
#[cfg(feature = "std")]
pub use driver::{guess_local_ip, IpSink, MagicSock};

use crate::crypto::NaclBox;
use crate::disco::{self, Message as DiscoMsg, TxId};
use crate::keys::{short, KeyPair};
use crate::stun;
use crate::time::{Instant, WallClock};
use crate::types::{DerpMap, DerpNode, Node, PeerChange};
use crate::wireguard::{self, Tunn};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::time::Duration;
use log::{debug, info, trace};

/// DERP "magic" IP used in `Node.DERP` and as the `src` of relayed pongs.
pub const DERP_MAGIC_IP: Ipv4Addr = Ipv4Addr::new(127, 3, 3, 40);

const TRUST_UDP_ADDR: Duration = Duration::from_millis(6500);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const SESSION_ACTIVE_TIMEOUT: Duration = Duration::from_secs(45);
const DISCO_PING_INTERVAL: Duration = Duration::from_secs(5);
/// A pong later than this after its ping is not counted.
const PING_ANSWER_WINDOW: Duration = Duration::from_secs(10);
/// Pings awaiting a pong per peer.
const MAX_PINGS: usize = if cfg!(feature = "small-ram") { 8 } else { 32 };
const CALL_ME_MAYBE_INTERVAL: Duration = Duration::from_secs(10);
const STUN_INTERVAL: Duration = Duration::from_secs(30);
const STUN_ANSWER_WINDOW: Duration = Duration::from_secs(5);
const DERP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Endpoints remembered per peer (from control plus call-me-maybe).
const MAX_ENDPOINTS: usize = if cfg!(feature = "small-ram") { 8 } else { 16 };
/// Endpoints kept per peer from the map with `small-ram`.
const SMALL_RAM_ENDPOINTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Udp(SocketAddr),
    Derp { region: i32, src: [u8; 32] },
}

/// A datagram for the caller to send.
#[derive(Debug, PartialEq, Eq)]
pub enum Transmit {
    /// Over UDP from the node's socket.
    Udp { dst: SocketAddr, data: Vec<u8> },
    /// Over the connection to DERP `region`, to the node with key `dst`.
    Derp {
        region: i32,
        dst: [u8; 32],
        data: Vec<u8>,
    },
}

/// Something for the caller to act on, other than a datagram to send.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// A decrypted IP packet from a peer, allowed by the packet filter.
    Ip(Vec<u8>),
    /// Open a connection to DERP `region` through `node`. Until it is closed,
    /// packets for the region arrive as [`Transmit::Derp`].
    DerpOpen { region: i32, node: DerpNode },
    /// Tell DERP `region` it is the node's home (and again after reconnects).
    DerpPreferred { region: i32 },
    /// Close the connection to DERP `region`.
    DerpClose { region: i32 },
    /// The endpoints to advertise to control changed.
    EndpointsChanged,
}

struct BestAddr {
    addr: SocketAddr,
    trust_until: Instant,
    latency: Duration,
}

struct Peer {
    /// The node ID control gives it (removals and patches refer to it).
    id: i64,
    key: [u8; 32],
    name: String,
    disco: Option<[u8; 32]>,
    allowed_ips: Vec<(IpAddr, u8)>,
    endpoints: Vec<SocketAddr>,
    home_derp: i32,
    /// Made the first time the peer is talked to: most peers of a tailnet
    /// never are, and a tunnel costs a few hundred bytes.
    tunn: Option<Box<Tunn>>,
    best: Option<BestAddr>,
    /// Disco pings awaiting a pong: (txid, address, sent). Only recent
    /// ones are kept (unanswered pings to stale endpoints used to pile up in
    /// a table to its cap, kilobytes per active peer).
    pings: Vec<(TxId, SocketAddr, Instant)>,
    last_ping_round: Option<Instant>,
    last_heartbeat: Option<Instant>,
    last_cmm: Option<Instant>,
    last_activity: Option<Instant>,
    /// The NaCl box between our disco key and `disco`, with the disco key
    /// it was made for; made the first time the peer is pinged or pings.
    disco_box: Option<Box<([u8; 32], NaclBox)>>,
}

impl Peer {
    /// The box for disco messages with the peer's current disco key. The
    /// X25519 behind it took about 33 ms on an ESP32-S3; done for every
    /// ping, it held the core for up to 340 ms each second.
    fn disco_box(&mut self, ours: &KeyPair) -> Option<&NaclBox> {
        let theirs = self.disco?;
        if self.disco_box.as_ref().map(|b| b.0) != Some(theirs) {
            self.disco_box = Some(Box::new((theirs, NaclBox::new(ours.secret(), &theirs))));
        }
        self.disco_box.as_deref().map(|b| &b.1)
    }

    /// The peer's WireGuard tunnel, made the first time it is needed.
    fn tunnel(&mut self, local: &KeyPair) -> &mut Tunn {
        let key = self.key;
        self.tunn
            .get_or_insert_with(|| Box::new(Tunn::new(local.clone(), key)))
    }
}

#[derive(Debug, Clone, Default)]
pub struct PeerStatus {
    pub name: String,
    pub key_short: String,
    pub direct: Option<SocketAddr>,
    pub home_derp: i32,
    pub has_session: bool,
    pub active: bool,
}

/// The magicsock state machine. See the module documentation.
pub struct MagicCore {
    node_key: KeyPair,
    disco_key: KeyPair,
    wall: WallClock,
    udp_port: u16,
    lan_ip: Option<IpAddr>,
    /// Boxed, so that the table is a small block of keys and pointers
    /// rather than one block of every peer's state (tens of kilobytes on a
    /// tailnet of 50, more than a device without PSRAM has in one piece).
    peers: BTreeMap<[u8; 32], Box<Peer>>,
    /// WireGuard receiver index to peer. Must be pruned with the peers.
    by_index: BTreeMap<u32, [u8; 32]>,
    filter: Option<crate::filter::Filter>,
    derp_map: DerpMap,
    home_derp: i32,
    /// Open DERP connections and when each last carried a packet of ours.
    derps: BTreeMap<i32, Instant>,
    stun_pending: BTreeMap<TxId, (i32, Instant)>,
    stun_latency: BTreeMap<i32, Duration>,
    mapped_addr: Option<SocketAddr>,
    last_stun: Option<Instant>,
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

impl MagicCore {
    /// A node with no peers yet. `udp_port` is the port its UDP socket is
    /// bound to; `wall` dates WireGuard handshakes.
    pub fn new(node_key: KeyPair, disco_key: KeyPair, udp_port: u16, wall: WallClock) -> Self {
        MagicCore {
            node_key,
            disco_key,
            wall,
            udp_port,
            lan_ip: None,
            peers: BTreeMap::new(),
            by_index: BTreeMap::new(),
            filter: None,
            derp_map: DerpMap::default(),
            home_derp: 0,
            derps: BTreeMap::new(),
            stun_pending: BTreeMap::new(),
            stun_latency: BTreeMap::new(),
            mapped_addr: None,
            last_stun: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    // ---- Output ----

    /// The next datagram to send.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        let t = self.transmits.pop_front();
        // A STUN round to every region queues dozens at once; the room is
        // not kept for good.
        if t.is_none() && self.transmits.capacity() > 16 {
            self.transmits = VecDeque::new();
        }
        t
    }

    /// The next event. Take events before transmits: a [`Transmit::Derp`]
    /// can be for a connection a [`Event::DerpOpen`] asks for.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // ---- Identity and clock ----

    pub fn node_key(&self) -> &KeyPair {
        &self.node_key
    }

    pub fn disco_public(&self) -> &[u8; 32] {
        self.disco_key.public()
    }

    pub fn wall_clock(&self) -> WallClock {
        self.wall
    }

    /// Sets Unix time (from control, on a device without a clock of its
    /// own). A difference of a second or two is ignored, so that handshake
    /// timestamps do not step back by the rounding of control's time.
    pub fn set_unix_time(&mut self, now: Instant, unix: Duration) {
        let current = self.wall.unix(now);
        let diff = current.abs_diff(unix);
        if diff > Duration::from_secs(2) {
            debug!(
                "magicsock: wall clock off by {}s; set from control",
                diff.as_secs()
            );
            self.wall = WallClock::new(now, unix);
        }
    }

    /// Installs a new node key (after control rotated it): every tunnel is
    /// made again with the new static key, and the DERP connections, which
    /// are authenticated with it, are closed (the home one opens again at
    /// the next tick).
    pub fn set_node_key(&mut self, key: KeyPair) {
        info!("magicsock: node key rotated to {}", short(key.public()));
        self.node_key = key;
        self.by_index.clear();
        for p in self.peers.values_mut() {
            p.tunn = None;
            p.best = None;
        }
        for region in core::mem::take(&mut self.derps).into_keys() {
            self.events.push_back(Event::DerpClose { region });
        }
    }

    // ---- Endpoints and status ----

    /// The node's address on its LAN, if known.
    pub fn set_lan_ip(&mut self, ip: Option<IpAddr>) {
        if self.lan_ip != ip {
            self.lan_ip = ip;
            self.events.push_back(Event::EndpointsChanged);
        }
    }

    /// Endpoints to advertise to the control plane: LAN address and the
    /// STUN-mapped public address.
    pub fn local_endpoints(&self) -> Vec<SocketAddr> {
        let mut v = Vec::new();
        if let Some(ip) = self.lan_ip {
            v.push(SocketAddr::new(ip, self.udp_port));
        }
        if let Some(m) = self.mapped_addr {
            if !v.contains(&m) {
                v.push(m);
            }
        }
        v
    }

    pub fn home_derp(&self) -> i32 {
        self.home_derp
    }

    pub fn derp_latency(&self) -> BTreeMap<String, f64> {
        self.stun_latency
            .iter()
            .map(|(r, d)| (r.to_string(), d.as_secs_f64()))
            .collect()
    }

    pub fn peer_status(&self, now: Instant) -> Vec<PeerStatus> {
        let mut v: Vec<PeerStatus> = self
            .peers
            .values()
            .map(|p| PeerStatus {
                name: p.name.clone(),
                key_short: short(&p.key),
                direct: p
                    .best
                    .as_ref()
                    .filter(|b| b.trust_until > now)
                    .map(|b| b.addr),
                home_derp: p.home_derp,
                has_session: p.tunn.as_ref().is_some_and(|t| t.has_session()),
                active: p
                    .last_activity
                    .map(|t| now.duration_since(t) < SESSION_ACTIVE_TIMEOUT)
                    .unwrap_or(false),
            })
            .collect();
        // Unstable: a stable sort of more than 20 items reserves 4 KB of
        // stack, which overflowed a 5 KB shell thread on the ESP32-C3.
        v.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        v
    }

    // ---- Control-plane driven configuration ----

    pub fn set_derp_map(&mut self, map: DerpMap) {
        self.derp_map = map;
        self.last_stun = None; // re-probe soon
    }

    /// Replaces the full peer list.
    pub fn set_peers(&mut self, nodes: Vec<Box<Node>>) {
        let mut old = core::mem::take(&mut self.peers);
        self.by_index.clear();
        for n in nodes {
            let Some(key) = n.key else {
                continue;
            };
            let mut peer = old.remove(&key).unwrap_or_else(|| new_peer(key));
            apply_node(&mut peer, *n);
            for idx in peer.tunn.iter().flat_map(|t| t.local_indices()) {
                self.by_index.insert(idx, key);
            }
            self.peers.insert(key, peer);
        }
        debug!("magicsock: {} peers", self.peers.len());
    }

    /// Forgets the peers whose keys are not in `keys` (the end of a full
    /// list that came one peer at a time through [`MagicCore::peers_changed`]).
    pub fn retain_peers(&mut self, keys: &[[u8; 32]]) {
        let gone: Vec<[u8; 32]> = self
            .peers
            .keys()
            .filter(|k| !keys.contains(k))
            .copied()
            .collect();
        for key in gone {
            self.remove_peer(key);
        }
    }

    pub fn peers_changed(&mut self, nodes: Vec<Box<Node>>) {
        for n in nodes {
            let Some(key) = n.key else {
                continue;
            };
            // A node whose key changed is the same ID under a new key.
            if let Some(stale) = self.key_of(n.id).filter(|k| *k != key) {
                self.remove_peer(stale);
            }
            let mut peer = self.peers.remove(&key).unwrap_or_else(|| new_peer(key));
            apply_node(&mut peer, *n);
            self.peers.insert(key, peer);
        }
    }

    pub fn peers_removed(&mut self, ids: &[i64]) {
        for id in ids {
            if let Some(key) = self.key_of(*id) {
                self.remove_peer(key);
            }
        }
    }

    /// Installs the packet filter (ACL) received from control.
    pub fn set_packet_filter(&mut self, filter: crate::filter::Filter) {
        self.filter = Some(filter);
    }

    pub fn apply_patches(&mut self, patches: &[PeerChange]) {
        for pc in patches {
            let Some(key) = self.key_of(pc.node_id) else {
                continue;
            };
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            if pc.derp_region != 0 {
                peer.home_derp = pc.derp_region;
            }
            if let Some(eps) = &pc.endpoints {
                peer.endpoints = eps.iter().filter_map(|e| e.parse().ok()).collect();
            }
            if let Some(d) = &pc.disco_key {
                if let Some(dk) = crate::keys::parse_prefixed(crate::keys::DISCO_PUB_PREFIX, d) {
                    if peer.disco != Some(dk) {
                        peer.best = None;
                    }
                    peer.disco = Some(dk);
                }
            }
        }
    }

    /// The key of the peer with control's node ID `id`.
    fn key_of(&self, id: i64) -> Option<[u8; 32]> {
        self.peers.values().find(|p| p.id == id).map(|p| p.key)
    }

    fn remove_peer(&mut self, key: [u8; 32]) {
        if self.peers.remove(&key).is_some() {
            self.by_index.retain(|_, k| *k != key);
        }
    }

    // ---- Data path: netstack -> peer ----

    /// Sends an IP packet produced by the local stack to the right peer.
    pub fn send_ip(&mut self, pkt: &[u8], now: Instant) {
        let Some(dst) = ip_dst(pkt) else { return };
        let Some(key) = self.route(dst) else {
            trace!("magicsock: no peer for {dst}");
            return;
        };
        let (local, wall) = (&self.node_key, self.wall);
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        peer.last_activity = Some(now);
        let out = peer.tunnel(local).encapsulate(pkt, now, wall);
        self.refresh_indices(key);
        for m in out {
            self.send_wg(key, m, now);
        }
        self.maybe_start_discovery(key, now);
    }

    fn refresh_indices(&mut self, key: [u8; 32]) {
        if let Some(p) = self.peers.get(&key) {
            let current: Vec<u32> = p.tunn.iter().flat_map(|t| t.local_indices()).collect();
            self.by_index
                .retain(|idx, k| *k != key || current.contains(idx));
            for idx in current {
                self.by_index.insert(idx, key);
            }
        }
    }

    /// Sends a WireGuard packet to a peer over the best available path.
    fn send_wg(&mut self, key: [u8; 32], pkt: Vec<u8>, now: Instant) {
        let Some(peer) = self.peers.get(&key) else {
            return;
        };
        if let Some(b) = &peer.best {
            if b.trust_until > now {
                let dst = b.addr;
                self.transmits.push_back(Transmit::Udp { dst, data: pkt });
                return;
            }
        }
        if peer.home_derp != 0 {
            let region = peer.home_derp;
            self.derp_send(region, key, pkt, now);
        } else if let Some(&dst) = peer.endpoints.first() {
            // No DERP known (e.g. tests): try the first endpoint directly.
            self.transmits.push_back(Transmit::Udp { dst, data: pkt });
        }
    }

    fn send_disco(&mut self, key: [u8; 32], path: Path, msg: &DiscoMsg, now: Instant) {
        let ours = &self.disco_key;
        let Some(nacl) = self.peers.get_mut(&key).and_then(|p| p.disco_box(ours)) else {
            return;
        };
        let pkt = disco::seal(ours.public(), nacl, msg);
        self.send_on(key, path, pkt, now);
    }

    fn send_on(&mut self, key: [u8; 32], path: Path, pkt: Vec<u8>, now: Instant) {
        match path {
            Path::Udp(dst) => self.transmits.push_back(Transmit::Udp { dst, data: pkt }),
            Path::Derp { region, .. } => self.derp_send(region, key, pkt, now),
        }
    }

    fn derp_send(&mut self, region: i32, dst: [u8; 32], data: Vec<u8>, now: Instant) {
        if !self.open_derp(region, now) {
            trace!("magicsock: no DERP node for region {region}");
            return;
        }
        self.transmits
            .push_back(Transmit::Derp { region, dst, data });
    }

    /// Makes sure a connection to `region` is open (asking the caller to
    /// open it if not) and marks it used. False if the map has no node there.
    fn open_derp(&mut self, region: i32, now: Instant) -> bool {
        if let Some(t) = self.derps.get_mut(&region) {
            *t = now;
            return true;
        }
        let Some(node) = pick_derp_node(&self.derp_map, region) else {
            return false;
        };
        self.derps.insert(region, now);
        self.events.push_back(Event::DerpOpen { region, node });
        true
    }

    /// The caller could not open (or lost for good) the connection to
    /// `region`: it is asked for again when next needed.
    pub fn derp_closed(&mut self, region: i32) {
        self.derps.remove(&region);
    }

    /// Makes `region` the home DERP: connected, and told it is preferred.
    pub fn connect_home_derp(&mut self, region: i32, now: Instant) {
        self.home_derp = region;
        if self.open_derp(region, now) {
            self.events.push_back(Event::DerpPreferred { region });
        }
    }

    // ---- Received packets ----

    /// A datagram that arrived on the UDP socket from `from`.
    pub fn handle_udp(&mut self, from: SocketAddr, pkt: &[u8], now: Instant) {
        if stun::is_stun(pkt) {
            self.handle_stun(pkt, now);
        } else {
            self.handle_packet(Path::Udp(from), pkt, now);
        }
    }

    /// A packet that DERP `region` relayed from the node with key `src`.
    pub fn handle_derp(&mut self, region: i32, src: [u8; 32], pkt: &[u8], now: Instant) {
        self.handle_packet(Path::Derp { region, src }, pkt, now);
    }

    fn handle_packet(&mut self, path: Path, pkt: &[u8], now: Instant) {
        if disco::is_disco(pkt) {
            self.handle_disco(path, pkt, now);
            return;
        }
        let Some(t) = wireguard::message_type(pkt) else {
            trace!(
                "magicsock: dropping unknown packet ({} bytes) from {path:?}",
                pkt.len()
            );
            return;
        };
        match t {
            wireguard::MSG_INITIATION => {
                let local = &self.node_key;
                let ci = match wireguard::consume_initiation(local, pkt) {
                    Ok(ci) => ci,
                    Err(e) => {
                        trace!("magicsock: bad initiation from {path:?}: {e}");
                        return;
                    }
                };
                let key = ci.peer_public;
                let Some(peer) = self.peers.get_mut(&key) else {
                    debug!("magicsock: initiation from unknown peer {}", short(&key));
                    return;
                };
                match peer.tunnel(local).accept_initiation(ci, now) {
                    Ok(resp) => {
                        self.refresh_indices(key);
                        self.note_path(key, path, now);
                        self.send_on(key, path, resp, now);
                        debug!("magicsock: handshake from {} via {path:?}", short(&key));
                    }
                    Err(e) => trace!("magicsock: initiation rejected: {e}"),
                }
            }
            wireguard::MSG_RESPONSE => {
                let Some(key) = self.peer_for_index(pkt) else {
                    return;
                };
                let Some(peer) = self.peers.get_mut(&key) else {
                    return;
                };
                match peer.tunnel(&self.node_key).consume_response(pkt, now) {
                    Ok(flushed) => {
                        self.refresh_indices(key);
                        self.note_path(key, path, now);
                        debug!(
                            "magicsock: handshake with {} complete via {path:?}",
                            short(&key)
                        );
                        for m in flushed {
                            self.send_wg(key, m, now);
                        }
                    }
                    Err(e) => trace!("magicsock: bad response: {e}"),
                }
            }
            wireguard::MSG_TRANSPORT => {
                let Some(key) = self.peer_for_index(pkt) else {
                    return;
                };
                let Some(peer) = self.peers.get_mut(&key) else {
                    return;
                };
                match peer.tunnel(&self.node_key).consume_transport(pkt, now) {
                    Ok(Some(ip)) => {
                        peer.last_activity = Some(now);
                        if !allowed_source(peer, &ip) {
                            trace!(
                                "magicsock: packet from {} with disallowed source",
                                short(&key)
                            );
                            return;
                        }
                        // The packet is authenticated, so the path is
                        // trusted even when the ACL drops the payload.
                        self.note_path(key, path, now);
                        let allowed = match &self.filter {
                            Some(f) => f.allows(&ip),
                            None => false,
                        };
                        if !allowed {
                            trace!("magicsock: packet from {} denied by ACL", short(&key));
                            return;
                        }
                        self.events.push_back(Event::Ip(ip));
                    }
                    Ok(None) => self.note_path(key, path, now),
                    Err(e) => trace!("magicsock: transport error: {e}"),
                }
            }
            _ => {}
        }
    }

    /// The peer a response or transport message is for, by receiver index.
    fn peer_for_index(&mut self, pkt: &[u8]) -> Option<[u8; 32]> {
        let idx = wireguard::receiver_index(pkt)?;
        let Some(key) = self.by_index.get(&idx).copied() else {
            trace!("magicsock: packet for unknown index");
            return None;
        };
        if !self.peers.contains_key(&key) {
            self.by_index.remove(&idx);
            return None;
        }
        Some(key)
    }

    /// Learns that `path` carried an authenticated packet from `key`
    /// (WireGuard-style roaming when no disco-verified path exists).
    fn note_path(&mut self, key: [u8; 32], path: Path, now: Instant) {
        if let Path::Udp(addr) = path {
            let Some(peer) = self.peers.get_mut(&key) else {
                return;
            };
            match &mut peer.best {
                Some(b) if b.addr == addr => b.trust_until = now + TRUST_UDP_ADDR,
                Some(b) if b.trust_until > now => {}
                _ => {
                    debug!("magicsock: {} now direct via {addr} (roaming)", short(&key));
                    peer.best = Some(BestAddr {
                        addr,
                        trust_until: now + TRUST_UDP_ADDR,
                        latency: Duration::ZERO,
                    });
                }
            }
        }
    }

    fn handle_disco(&mut self, path: Path, pkt: &[u8], now: Instant) {
        let Some(sender) = disco::sender_key(pkt) else {
            return;
        };
        // A linear search: a tailnet has tens of peers, and a table for this
        // cost a block of memory that had to grow as peers arrived.
        let Some(key) = self
            .peers
            .iter()
            .find(|(_, p)| p.disco == Some(sender))
            .map(|(k, _)| *k)
        else {
            trace!("magicsock: disco from unknown key {}", short(&sender));
            return;
        };
        let ours = &self.disco_key;
        let opened = self
            .peers
            .get_mut(&key)
            .and_then(|p| p.disco_box(ours))
            .map(|b| disco::open(b, pkt));
        let msg = match opened.unwrap_or(Err(crate::crypto::CryptoError)) {
            Ok(m) => m,
            Err(_) => {
                trace!("magicsock: disco open failed from {path:?}");
                return;
            }
        };
        match msg {
            DiscoMsg::Ping { txid, .. } => {
                let src = match path {
                    Path::Udp(a) => a,
                    Path::Derp { region, .. } => {
                        SocketAddr::new(IpAddr::V4(DERP_MAGIC_IP), region as u16)
                    }
                };
                trace!("magicsock: ping from {} via {path:?}", short(&key));
                self.send_disco(key, path, &DiscoMsg::Pong { txid, src }, now);
                if let Path::Udp(addr) = path {
                    // A direct ping reached us: probe the sender's address back.
                    let Some(peer) = self.peers.get(&key) else {
                        return;
                    };
                    let known_good = peer
                        .best
                        .as_ref()
                        .map(|b| b.addr == addr && b.trust_until > now)
                        .unwrap_or(false);
                    if !known_good {
                        self.send_ping(key, addr, now);
                    }
                }
            }
            DiscoMsg::Pong { txid, src: _ } => {
                let Some(peer) = self.peers.get_mut(&key) else {
                    return;
                };
                let found = peer.pings.iter().position(|(t, _, _)| *t == txid);
                if let Some((_, addr, sent_at)) = found.map(|i| peer.pings.swap_remove(i)) {
                    let latency = now.duration_since(sent_at);
                    let better = match &peer.best {
                        Some(b) => {
                            b.trust_until <= now
                                || b.addr == addr
                                || latency + Duration::from_millis(5) < b.latency
                        }
                        None => true,
                    };
                    if better {
                        if peer.best.as_ref().map(|b| b.addr != addr).unwrap_or(true) {
                            info!(
                                "magicsock: {} ({}) direct via {addr} ({latency:?})",
                                peer.name,
                                short(&key)
                            );
                        }
                        peer.best = Some(BestAddr {
                            addr,
                            trust_until: now + TRUST_UDP_ADDR,
                            latency,
                        });
                    }
                }
            }
            DiscoMsg::CallMeMaybe { endpoints } => {
                let Some(peer) = self.peers.get_mut(&key) else {
                    return;
                };
                for ep in &endpoints {
                    if !peer.endpoints.contains(ep) && peer.endpoints.len() < MAX_ENDPOINTS {
                        peer.endpoints.push(*ep);
                    }
                }
                debug!(
                    "magicsock: call-me-maybe from {} with {} endpoints",
                    short(&key),
                    endpoints.len()
                );
                self.ping_all_endpoints(key, now);
            }
        }
    }

    fn send_ping(&mut self, key: [u8; 32], addr: SocketAddr, now: Instant) {
        let txid = disco::new_txid();
        {
            let Some(peer) = self.peers.get_mut(&key) else {
                return;
            };
            peer.pings
                .retain(|(_, _, t)| now.duration_since(*t) < PING_ANSWER_WINDOW);
            if peer.pings.len() >= MAX_PINGS {
                peer.pings.remove(0);
            }
            peer.pings.push((txid, addr, now));
        }
        let msg = DiscoMsg::Ping {
            txid,
            node_key: Some(*self.node_key.public()),
        };
        self.send_disco(key, Path::Udp(addr), &msg, now);
    }

    fn ping_all_endpoints(&mut self, key: [u8; 32], now: Instant) {
        let eps: Vec<SocketAddr> = match self.peers.get_mut(&key) {
            Some(p) => {
                p.last_ping_round = Some(now);
                p.endpoints
                    .iter()
                    .copied()
                    .filter(|e| !e.ip().is_unspecified() && e.ip() != IpAddr::V4(DERP_MAGIC_IP))
                    .collect()
            }
            None => return,
        };
        for ep in eps {
            self.send_ping(key, ep, now);
        }
    }

    fn send_call_me_maybe(&mut self, key: [u8; 32], now: Instant) {
        let region = match self.peers.get_mut(&key) {
            Some(p) if p.home_derp != 0 && p.disco.is_some() => {
                p.last_cmm = Some(now);
                p.home_derp
            }
            _ => return,
        };
        let endpoints = self.local_endpoints();
        if endpoints.is_empty() {
            return;
        }
        let msg = DiscoMsg::CallMeMaybe { endpoints };
        self.send_disco(key, Path::Derp { region, src: key }, &msg, now);
    }

    /// Kicks off discovery when traffic starts and no direct path exists.
    fn maybe_start_discovery(&mut self, key: [u8; 32], now: Instant) {
        let (has_best, ping_due, cmm_due) = match self.peers.get(&key) {
            Some(p) => (
                p.best
                    .as_ref()
                    .map(|b| b.trust_until > now)
                    .unwrap_or(false),
                p.last_ping_round
                    .map(|t| now.duration_since(t) >= DISCO_PING_INTERVAL)
                    .unwrap_or(true),
                p.last_cmm
                    .map(|t| now.duration_since(t) >= CALL_ME_MAYBE_INTERVAL)
                    .unwrap_or(true),
            ),
            None => return,
        };
        if has_best {
            return;
        }
        if ping_due {
            self.ping_all_endpoints(key, now);
        }
        if cmm_due {
            self.send_call_me_maybe(key, now);
        }
    }

    // ---- Timers ----

    /// Periodic work, about once a second: handshake retries and keepalives,
    /// path upkeep, STUN, and the DERP connections (home kept, idle closed).
    pub fn tick(&mut self, now: Instant) {
        let keys: Vec<[u8; 32]> = self.peers.keys().copied().collect();
        let wall = self.wall;
        for key in keys {
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            let out = peer
                .tunn
                .as_mut()
                .map(|t| t.tick(now, wall))
                .unwrap_or_default();
            self.refresh_indices(key);
            for m in out {
                self.send_wg(key, m, now);
            }
            self.peer_path_maintenance(key, now);
        }
        self.stun_maintenance(now);
        let home = self.home_derp;
        if home != 0 && !self.derps.contains_key(&home) {
            // The home connection is missing (the map arrived late, the key
            // was rotated, or it could not be opened); bring it back.
            self.connect_home_derp(home, now);
        }
        let idle: Vec<i32> = self
            .derps
            .iter()
            .filter(|(r, t)| **r != home && now.duration_since(**t) > DERP_IDLE_TIMEOUT)
            .map(|(r, _)| *r)
            .collect();
        for region in idle {
            self.derps.remove(&region);
            self.events.push_back(Event::DerpClose { region });
            debug!("magicsock: closed idle DERP {region}");
        }
    }

    fn peer_path_maintenance(&mut self, key: [u8; 32], now: Instant) {
        let (active, best_addr, heartbeat_due) = match self.peers.get_mut(&key) {
            Some(p) => {
                let active = p
                    .last_activity
                    .map(|t| now.duration_since(t) < SESSION_ACTIVE_TIMEOUT)
                    .unwrap_or(false);
                if let Some(b) = &p.best {
                    if b.trust_until <= now && active {
                        debug!(
                            "magicsock: {} direct path {} expired; falling back to DERP",
                            short(&key),
                            b.addr
                        );
                        p.best = None;
                    }
                }
                let hb = p
                    .last_heartbeat
                    .map(|t| now.duration_since(t) >= HEARTBEAT_INTERVAL)
                    .unwrap_or(true);
                (active, p.best.as_ref().map(|b| b.addr), hb)
            }
            None => return,
        };
        if !active {
            return;
        }
        match best_addr {
            Some(addr) => {
                if heartbeat_due {
                    if let Some(p) = self.peers.get_mut(&key) {
                        p.last_heartbeat = Some(now);
                    }
                    self.send_ping(key, addr, now);
                }
            }
            None => self.maybe_start_discovery(key, now),
        }
    }

    // ---- STUN ----

    fn stun_maintenance(&mut self, now: Instant) {
        self.stun_pending
            .retain(|_, (_, t)| now.duration_since(*t) < STUN_ANSWER_WINDOW);
        let due = self
            .last_stun
            .map(|t| now.duration_since(t) >= STUN_INTERVAL)
            .unwrap_or(true);
        if !due || self.derp_map.regions.is_empty() {
            return;
        }
        self.last_stun = Some(now);
        let home = self.home_derp;
        let probe_all = home == 0 || self.stun_latency.is_empty();
        let targets: Vec<(i32, SocketAddr)> = self
            .derp_map
            .regions
            .values()
            .filter(|r| !r.avoid || r.region_id == home)
            // Probe every region until we have latencies, then only home.
            .filter(|r| probe_all || r.region_id == home)
            .filter_map(|r| {
                let n = r.nodes.iter().find(|n| n.ipv4.is_some())?;
                let ip: Ipv4Addr = n.ipv4?;
                let port = if n.stun_port > 0 {
                    n.stun_port as u16
                } else {
                    3478
                };
                Some((r.region_id, SocketAddr::new(IpAddr::V4(ip), port)))
            })
            .collect();
        for (region, dst) in targets {
            let txid = stun::new_txid();
            self.stun_pending.insert(txid, (region, now));
            self.transmits.push_back(Transmit::Udp {
                dst,
                data: stun::request(&txid),
            });
        }
    }

    fn handle_stun(&mut self, pkt: &[u8], now: Instant) {
        let Some((txid, mapped)) = stun::parse_response(pkt) else {
            return;
        };
        let Some((region, sent)) = self.stun_pending.remove(&txid) else {
            return;
        };
        let rtt = now.duration_since(sent);
        self.stun_latency.insert(region, rtt);
        trace!("magicsock: stun region {region} rtt {rtt:?} mapped {mapped}");
        if self.mapped_addr != Some(mapped) {
            info!("magicsock: public endpoint {mapped} (via DERP region {region})");
            self.mapped_addr = Some(mapped);
            self.events.push_back(Event::EndpointsChanged);
        }
    }

    /// Sends STUN probes to every region now, forgetting earlier latencies.
    /// Read the answer with [`MagicCore::nearest_region`] a moment later.
    pub fn probe_derp_regions(&mut self, now: Instant) {
        self.last_stun = None;
        self.stun_latency.clear();
        self.stun_maintenance(now);
    }

    /// The region whose STUN answer came back fastest.
    pub fn nearest_region(&self) -> Option<i32> {
        self.stun_latency
            .iter()
            .min_by_key(|(_, d)| **d)
            .map(|(r, _)| *r)
    }

    /// Fallback home-region choice when STUN is blocked: prefer a region by
    /// code (e.g. "tok"), else the lowest region id.
    pub fn fallback_region(&self, preferred_code: &str) -> Option<i32> {
        let mut regions: Vec<&crate::types::DerpRegion> = self
            .derp_map
            .regions
            .values()
            .filter(|r| !r.avoid && !r.nodes.is_empty())
            .collect();
        if let Some(r) = regions.iter().find(|r| r.region_code == preferred_code) {
            return Some(r.region_id);
        }
        regions.sort_unstable_by_key(|r| r.region_id);
        regions.first().map(|r| r.region_id)
    }

    fn route(&self, dst: IpAddr) -> Option<[u8; 32]> {
        let mut best: Option<(u8, [u8; 32])> = None;
        for (key, p) in &self.peers {
            for c in &p.allowed_ips {
                if cidr_contains(c, dst) && best.map(|(b, _)| c.1 > b).unwrap_or(true) {
                    best = Some((c.1, *key));
                }
            }
        }
        best.map(|(_, k)| k)
    }
}

fn new_peer(key: [u8; 32]) -> Box<Peer> {
    Box::new(Peer {
        id: 0,
        key,
        name: String::new(),
        disco: None,
        allowed_ips: Vec::new(),
        endpoints: Vec::new(),
        home_derp: 0,
        tunn: None,
        best: None,
        pings: Vec::new(),
        last_ping_round: None,
        last_heartbeat: None,
        last_cmm: None,
        last_activity: None,
        disco_box: None,
    })
}

/// Takes the node's fields over, so that the parsed map and the peer table
/// are not both held in full.
fn apply_node(peer: &mut Peer, n: Node) {
    peer.id = n.id;
    let Node {
        mut name,
        disco_key,
        addresses,
        mut allowed_ips,
        endpoints,
        ..
    } = n;
    let home_derp = [n.home_derp, n.derp]
        .into_iter()
        .find(|r| *r != 0)
        .unwrap_or(0);
    if name.ends_with('.') {
        name.pop();
    }
    name.shrink_to_fit();
    peer.name = name;
    if disco_key != peer.disco {
        peer.best = None;
    }
    peer.disco = disco_key;
    // Prefix-length-0 routes (exit nodes) are not installed: they would let
    // that peer send with any source address and swallow every destination.
    allowed_ips.extend(addresses);
    allowed_ips.retain(|c| c.1 != 0);
    allowed_ips.dedup();
    allowed_ips.shrink_to_fit();
    peer.allowed_ips = allowed_ips;
    peer.endpoints = endpoints;
    // With `small-ram` only the first few are kept: a peer can list a dozen
    // (every interface address), and the public ones the client found by
    // STUN come first.
    if cfg!(feature = "small-ram") {
        peer.endpoints.truncate(SMALL_RAM_ENDPOINTS);
    }
    peer.endpoints.shrink_to_fit();
    peer.home_derp = home_derp;
}

fn cidr_contains(cidr: &(IpAddr, u8), ip: IpAddr) -> bool {
    match (cidr.0, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let bits = cidr.1.min(32);
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            (u32::from(net) & mask) == (u32::from(ip) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let bits = cidr.1.min(128);
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            (u128::from(net) & mask) == (u128::from(ip) & mask)
        }
        _ => false,
    }
}

fn allowed_source(peer: &Peer, ip_pkt: &[u8]) -> bool {
    match ip_src(ip_pkt) {
        Some(src) => peer.allowed_ips.iter().any(|c| cidr_contains(c, src)),
        None => false,
    }
}

fn ip_dst(pkt: &[u8]) -> Option<IpAddr> {
    match pkt.first()? >> 4 {
        4 if pkt.len() >= 20 => Some(IpAddr::V4(Ipv4Addr::new(
            pkt[16], pkt[17], pkt[18], pkt[19],
        ))),
        6 if pkt.len() >= 40 => {
            let o: [u8; 16] = pkt[24..40].try_into().ok()?;
            Some(IpAddr::V6(o.into()))
        }
        _ => None,
    }
}

fn ip_src(pkt: &[u8]) -> Option<IpAddr> {
    match pkt.first()? >> 4 {
        4 if pkt.len() >= 20 => Some(IpAddr::V4(Ipv4Addr::new(
            pkt[12], pkt[13], pkt[14], pkt[15],
        ))),
        6 if pkt.len() >= 40 => {
            let o: [u8; 16] = pkt[8..24].try_into().ok()?;
            Some(IpAddr::V6(o.into()))
        }
        _ => None,
    }
}

fn pick_derp_node(map: &DerpMap, region: i32) -> Option<DerpNode> {
    let r = map.regions.get(&region)?;
    r.nodes.iter().find(|n| !n.stun_only).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::parse_cidr;
    use alloc::format;
    use alloc::vec;

    fn node(id: i64, key: u8, disco: u8) -> Box<Node> {
        Box::new(Node {
            id,
            name: format!("n{id}."),
            key: Some([key; 32]),
            disco_key: Some([disco; 32]),
            ..Default::default()
        })
    }

    fn core_at(port: u16) -> MagicCore {
        let mut c = MagicCore::new(
            KeyPair::generate(),
            KeyPair::generate(),
            port,
            WallClock::default(),
        );
        c.set_lan_ip(Some("127.0.0.1".parse().unwrap()));
        c
    }

    #[test]
    fn peers_are_found_by_node_id_for_changes_removals_and_patches() {
        let mut ms = core_at(1);
        let keys = |ms: &MagicCore| {
            let mut k: Vec<(i64, u8, Option<u8>)> = ms
                .peers
                .values()
                .map(|p| (p.id, p.key[0], p.disco.map(|d| d[0])))
                .collect();
            k.sort();
            k
        };
        ms.set_peers(vec![node(1, 0x11, 0xd1), node(2, 0x22, 0xd2)]);
        assert_eq!(keys(&ms), [(1, 0x11, Some(0xd1)), (2, 0x22, Some(0xd2))]);
        // Node 2 comes back with a new key: the old entry goes.
        ms.peers_changed(vec![node(2, 0x33, 0xd3)]);
        assert_eq!(keys(&ms), [(1, 0x11, Some(0xd1)), (2, 0x33, Some(0xd3))]);
        // A patch reaches node 1 by its ID.
        ms.apply_patches(&[PeerChange {
            node_id: 1,
            disco_key: Some(format!("discokey:{}", "e1".repeat(32))),
            ..Default::default()
        }]);
        assert_eq!(keys(&ms)[0], (1, 0x11, Some(0xe1)));
        ms.peers_removed(&[1, 99]);
        assert_eq!(keys(&ms), [(2, 0x33, Some(0xd3))]);
    }

    fn peer_node(of: &MagicCore, name: &str, ip: &str, port: u16) -> Box<Node> {
        Box::new(Node {
            id: 1,
            name: name.into(),
            key: Some(*of.node_key().public()),
            disco_key: Some(*of.disco_public()),
            addresses: vec![(ip.parse().unwrap(), 32)],
            allowed_ips: vec![(ip.parse().unwrap(), 32)],
            endpoints: vec![format!("127.0.0.1:{port}").parse().unwrap()],
            ..Default::default()
        })
    }

    /// A UDP-in-IPv4 packet from `src` to `dst`, port 1000 to 2000.
    fn ipv4(src: [u8; 4], dst: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 28];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&((28 + payload.len()) as u16).to_be_bytes());
        p[9] = 17;
        p[12..16].copy_from_slice(&src);
        p[16..20].copy_from_slice(&dst);
        p[20..22].copy_from_slice(&1000u16.to_be_bytes());
        p[22..24].copy_from_slice(&2000u16.to_be_bytes());
        p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// Moves every UDP datagram between two cores (on ports 1 and 2) until
    /// neither has more to send. Returns the IP packets each received.
    fn exchange(
        a: &mut MagicCore,
        b: &mut MagicCore,
        now: Instant,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let (pa, pb): (SocketAddr, SocketAddr) = (
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        );
        let (mut ip_a, mut ip_b) = (Vec::new(), Vec::new());
        loop {
            let mut moved = false;
            while let Some(t) = a.poll_transmit() {
                if let Transmit::Udp { dst, data } = t {
                    if dst == pb {
                        b.handle_udp(pa, &data, now);
                        moved = true;
                    }
                }
            }
            while let Some(t) = b.poll_transmit() {
                if let Transmit::Udp { dst, data } = t {
                    if dst == pa {
                        a.handle_udp(pb, &data, now);
                        moved = true;
                    }
                }
            }
            for (core, ips) in [(&mut *a, &mut ip_a), (&mut *b, &mut ip_b)] {
                while let Some(e) = core.poll_event() {
                    if let Event::Ip(p) = e {
                        ips.push(p);
                    }
                }
            }
            if !moved {
                return (ip_a, ip_b);
            }
        }
    }

    /// Two cores and no I/O: a handshake, a packet each way, a direct path
    /// confirmed by disco, and the ACL dropping what it does not allow.
    #[test]
    fn two_cores_talk_through_wireguard_without_io() {
        let (mut a, mut b) = (core_at(1), core_at(2));
        a.set_peers(vec![peer_node(&b, "b", "100.64.0.2", 2)]);
        b.set_peers(vec![peer_node(&a, "a", "100.64.0.1", 1)]);
        b.set_packet_filter(crate::filter::Filter::allow_all());
        let now = Instant::from_millis(10_000);

        let hello = ipv4([100, 64, 0, 1], [100, 64, 0, 2], b"hello");
        a.send_ip(&hello, now);
        let (got_a, got_b) = exchange(&mut a, &mut b, now);
        assert!(got_a.is_empty());
        assert_eq!(got_b, [hello]);

        // a has no filter yet: deny all.
        let reply = ipv4([100, 64, 0, 2], [100, 64, 0, 1], b"reply");
        b.send_ip(&reply, now);
        assert_eq!(exchange(&mut a, &mut b, now), (vec![], vec![]));
        a.set_packet_filter(crate::filter::Filter::allow_all());
        b.send_ip(&reply, now);
        assert_eq!(exchange(&mut a, &mut b, now).0, [reply]);

        // The pings that went with the first packet made the path direct.
        let st = a.peer_status(now);
        assert_eq!(st[0].direct, Some("127.0.0.1:2".parse().unwrap()));
        assert!(st[0].has_session && st[0].active);
        // Trust runs out without traffic; the tick sends a heartbeat ping
        // while the peer is active, and its pong renews it.
        let later = now + Duration::from_secs(4);
        a.tick(later);
        exchange(&mut a, &mut b, later);
        assert!(a.peer_status(later + Duration::from_secs(5))[0]
            .direct
            .is_some());
    }

    #[test]
    fn derp_connections_open_on_use_and_close_when_idle() {
        let mut c = core_at(1);
        let mut map = DerpMap::default();
        for id in [1, 2] {
            map.regions.insert(
                id,
                crate::types::DerpRegion {
                    region_id: id,
                    nodes: vec![DerpNode {
                        region_id: id,
                        host_name: format!("derp{id}"),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            );
        }
        c.set_derp_map(map);
        let t0 = Instant::from_millis(1000);
        c.connect_home_derp(1, t0);
        let mut peer = node(5, 0x55, 0xd5);
        peer.home_derp = 2;
        peer.allowed_ips = vec![("100.64.0.5".parse().unwrap(), 32)];
        c.set_peers(vec![peer]);
        c.send_ip(&ipv4([100, 64, 0, 1], [100, 64, 0, 5], b"x"), t0);
        let events: Vec<Event> = core::iter::from_fn(|| c.poll_event()).collect();
        let opened: Vec<i32> = events
            .iter()
            .filter_map(|e| match e {
                Event::DerpOpen { region, .. } => Some(*region),
                _ => None,
            })
            .collect();
        assert_eq!(opened, [1, 2]);
        assert!(events.contains(&Event::DerpPreferred { region: 1 }));
        // The initiation went to the peer's home region.
        assert!(core::iter::from_fn(|| c.poll_transmit())
            .any(|t| matches!(t, Transmit::Derp { region: 2, dst, .. } if dst == [0x55; 32])));
        // Once nothing goes there for a minute, the peer's region closes;
        // home stays.
        c.peers_removed(&[5]);
        c.tick(t0 + Duration::from_secs(61));
        let events: Vec<Event> = core::iter::from_fn(|| c.poll_event()).collect();
        assert!(events.contains(&Event::DerpClose { region: 2 }));
        assert!(!events.contains(&Event::DerpClose { region: 1 }));
        // A rotated key closes every connection; the next tick reopens home.
        c.set_node_key(KeyPair::generate());
        assert!(core::iter::from_fn(|| c.poll_event()).any(|e| e == Event::DerpClose { region: 1 }));
        c.tick(t0 + Duration::from_secs(62));
        assert!(core::iter::from_fn(|| c.poll_event())
            .any(|e| matches!(e, Event::DerpOpen { region: 1, .. })));
    }

    #[test]
    fn cidr_and_routing() {
        assert!(cidr_contains(
            &("100.64.0.0".parse().unwrap(), 10),
            "100.100.1.1".parse().unwrap()
        ));
        assert!(!cidr_contains(
            &("100.64.0.0".parse().unwrap(), 10),
            "100.128.0.1".parse().unwrap()
        ));
        assert!(cidr_contains(
            &("fd7a:115c:a1e0::".parse().unwrap(), 48),
            "fd7a:115c:a1e0::1".parse().unwrap()
        ));
        assert_eq!(
            parse_cidr("100.64.0.5/32"),
            Some(("100.64.0.5".parse().unwrap(), 32))
        );
    }
}
