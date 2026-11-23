//! The node: registration, the map stream and what it changes.
//!
//! [`NodeCore`] makes the decisions and does no I/O. Its caller sends the
//! requests it builds ([`NodeCore::register_request`],
//! [`NodeCore::map_request`]) over a control connection, hands the answers
//! back, and carries out the [`Action`]s it asks for (store a value, set the
//! stack's addresses). It configures the [`MagicCore`] it is handed.
//!
//! `Node` (`std`) runs it on a control thread together with a `MagicSock`
//! and a `NetStack`.

#[cfg(feature = "std")]
mod driver;
#[cfg(feature = "std")]
pub use driver::{Node, NodeConfig, Store};

use crate::filter::Filter;
use crate::keys::{fmt_disco_pub, fmt_node_pub, KeyPair};
use crate::magicsock::{MagicCore, PeerStatus};
use crate::time::Instant;
use crate::types::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::net::{IpAddr, SocketAddr};
use core::time::Duration;
use log::{debug, info, warn};

pub const KEY_MACHINE: &str = "machine_key";
pub const KEY_NODE: &str = "node_key";
pub const KEY_DISCO: &str = "disco_key";
pub const KEY_REGISTERED: &str = "registered";
pub const KEY_HOME_DERP: &str = "home_derp";

/// The control server sends keep-alives about once a minute on a map stream.
const MAP_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(150);
/// Endpoint updates are sent at most this often.
const LITE_UPDATE_INTERVAL: Duration = Duration::from_secs(3);
/// How long STUN answers are waited for before a home region is chosen.
const HOME_PROBE_WAIT: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeState {
    Starting,
    ConnectingControl,
    Registering,
    /// Registration requires interactive approval at the given URL.
    NeedsLogin(String),
    Running,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct NodeStatus {
    pub state: NodeState,
    pub addresses: Vec<IpAddr>,
    pub dns_name: String,
    pub home_derp: i32,
    pub derp_connected: bool,
    pub endpoints: Vec<SocketAddr>,
    pub peers: Vec<PeerStatus>,
    pub last_error: Option<String>,
    pub map_updates: u64,
}

/// What the node's identity and registration are made of.
#[derive(Clone, Debug)]
pub struct NodeSettings {
    pub hostname: String,
    pub version: String,
    /// Auth key for headless registration; absent once registered.
    pub auth_key: Option<String>,
    /// DERP region code used when STUN gets no answers (e.g. "tok").
    pub fallback_region_code: String,
}

/// Something the caller must do for the node.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep `value` under `key` across restarts (`None`: forget it).
    Store {
        key: &'static str,
        value: Option<String>,
    },
    /// The node's tailnet addresses, with their prefixes, for the stack.
    Addresses(Vec<(IpAddr, u8)>),
}

/// How a registration request ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Registration {
    /// Registered: go on to the map stream.
    Authorized,
    /// Send [`NodeCore::register_request`] again now (the node key expired
    /// and a new one is being registered).
    Again,
    /// Waiting for someone to approve the node (see [`NodeState::NeedsLogin`]);
    /// ask again later.
    Waiting,
}

/// The node's decisions, without I/O. See the module documentation.
pub struct NodeCore {
    settings: NodeSettings,
    node_key: KeyPair,
    /// A new node key being registered in place of an expired one.
    pending_key: Option<KeyPair>,
    registered: bool,
    /// The home DERP region remembered from an earlier run.
    saved_home: Option<i32>,
    state: NodeState,
    last_error: Option<String>,
    addresses: Vec<IpAddr>,
    dns_name: String,
    map_updates: u64,
    /// Named ACL rule sets as received from control (`*` is the base set).
    filter_sets: BTreeMap<String, Vec<FilterRule>>,
    endpoints_dirty: bool,
    last_lite: Option<Instant>,
    last_msg: Option<Instant>,
    /// When to choose a home region from the STUN answers.
    home_probe_due: Option<Instant>,
    /// Keys of a full peer list arriving one peer at a time
    /// ([`NodeCore::on_peer_event`]).
    full_peers: Vec<[u8; 32]>,
    actions: VecDeque<Action>,
}

impl NodeCore {
    /// `registered` and `saved_home` are what the store remembers.
    pub fn new(
        settings: NodeSettings,
        node_key: KeyPair,
        registered: bool,
        saved_home: Option<i32>,
    ) -> Self {
        NodeCore {
            settings,
            node_key,
            pending_key: None,
            registered,
            saved_home,
            state: NodeState::Starting,
            last_error: None,
            addresses: Vec::new(),
            dns_name: String::new(),
            map_updates: 0,
            filter_sets: BTreeMap::new(),
            endpoints_dirty: false,
            last_lite: None,
            last_msg: None,
            home_probe_due: None,
            full_peers: Vec::new(),
            actions: VecDeque::new(),
        }
    }

    /// The next thing to do for the node.
    pub fn poll_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    pub fn state(&self) -> &NodeState {
        &self.state
    }

    pub fn node_key(&self) -> &KeyPair {
        &self.node_key
    }

    /// The status, as far as the node and `magic` know it
    /// (`derp_connected` is the caller's to fill in).
    pub fn status(&self, magic: &MagicCore, now: Instant) -> NodeStatus {
        NodeStatus {
            state: self.state.clone(),
            addresses: self.addresses.clone(),
            dns_name: self.dns_name.clone(),
            home_derp: magic.home_derp(),
            derp_connected: false,
            endpoints: magic.local_endpoints(),
            peers: magic.peer_status(now),
            last_error: self.last_error.clone(),
            map_updates: self.map_updates,
        }
    }

    fn set_state(&mut self, state: NodeState) {
        if self.state != state {
            info!("node: state {:?}", state);
            if let NodeState::Error(e) = &state {
                self.last_error = Some(e.clone());
            }
            self.state = state;
        }
    }

    /// A control connection is being opened.
    pub fn connecting(&mut self) {
        self.set_state(NodeState::ConnectingControl);
    }

    /// Something failed before the map stream ran (`what`: a description).
    pub fn failed(&mut self, what: String) {
        self.set_state(NodeState::Error(what));
    }

    // ---- Registration ----

    pub fn needs_registration(&self) -> bool {
        !self.registered
    }

    /// The `/machine/register` request to send.
    pub fn register_request(&mut self) -> RegisterRequest {
        self.set_state(NodeState::Registering);
        let current = fmt_node_pub(self.node_key.public());
        let (node_key, old_node_key) = match &self.pending_key {
            Some(new) => (fmt_node_pub(new.public()), current),
            None => (current, fmt_node_pub(&[0u8; 32])),
        };
        let auth = self.settings.auth_key.clone().filter(|k| !k.is_empty());
        RegisterRequest {
            version: CAPABILITY_VERSION,
            node_key,
            old_node_key,
            auth: auth.map(|k| RegisterResponseAuth { auth_key: k }),
            expiry: ZERO_TIME.into(),
            followup: String::new(),
            hostinfo: hostinfo(&self.settings.hostname, &self.settings.version, 0),
            ephemeral: false,
        }
    }

    /// Takes the answer to [`register_request`](Self::register_request).
    /// A rotated node key goes to `magic`. `Err` holds control's reason.
    pub fn on_register_response(
        &mut self,
        resp: RegisterResponse,
        magic: &mut MagicCore,
    ) -> Result<Registration, String> {
        if !resp.error.is_empty() {
            self.pending_key = None;
            return Err(resp.error);
        }
        if let Some(new_key) = self.pending_key.take() {
            self.actions.push_back(Action::Store {
                key: KEY_NODE,
                value: Some(new_key.secret_hex()),
            });
            self.node_key = new_key.clone();
            magic.set_node_key(new_key);
            return Ok(self.authorized(resp.machine_authorized));
        }
        if resp.node_key_expired {
            info!("node: node key expired; rotating");
            self.pending_key = Some(KeyPair::generate());
            return Ok(Registration::Again);
        }
        if resp.machine_authorized {
            info!("node: registered as {}", resp.login.login_name);
            return Ok(self.authorized(true));
        }
        if !resp.auth_url.is_empty() {
            warn!("node: approval required: {}", resp.auth_url);
            self.set_state(NodeState::NeedsLogin(resp.auth_url));
            return Ok(Registration::Waiting);
        }
        Err("not authorized and no auth URL".into())
    }

    fn authorized(&mut self, yes: bool) -> Registration {
        if !yes {
            return Registration::Waiting;
        }
        self.registered = true;
        self.actions.push_back(Action::Store {
            key: KEY_REGISTERED,
            value: Some("1".into()),
        });
        Registration::Authorized
    }

    // ---- The map stream ----

    /// A `/machine/map` request: the long-poll stream, or (`stream`
    /// false) an endpoint update that leaves the peers out.
    pub fn map_request(&self, stream: bool, magic: &MagicCore) -> MapRequest {
        let home = magic.home_derp();
        let mut hi = hostinfo(&self.settings.hostname, &self.settings.version, home);
        if let Some(ni) = hi.net_info.as_mut() {
            ni.derp_latency = magic.derp_latency();
        }
        MapRequest {
            version: CAPABILITY_VERSION,
            compress: String::new(),
            keep_alive: stream,
            node_key: fmt_node_pub(self.node_key.public()),
            disco_key: fmt_disco_pub(magic.disco_public()),
            stream,
            hostinfo: hi,
            endpoints: magic
                .local_endpoints()
                .iter()
                .map(|e| e.to_string())
                .collect(),
            omit_peers: !stream,
            read_only: false,
        }
    }

    /// Before the map stream is requested: reconnects to the home DERP of
    /// an earlier run so that peers can reach the node early.
    pub fn starting_map(&mut self, magic: &mut MagicCore, now: Instant) {
        // What is left of a list a broken stream cut short.
        self.full_peers.clear();
        if magic.home_derp() == 0 {
            if let Some(r) = self.saved_home {
                magic.connect_home_derp(r, now);
            }
        }
    }

    /// The map stream is open.
    pub fn map_connected(&mut self, now: Instant) {
        info!("node: map stream connected");
        self.set_state(NodeState::Running);
        self.endpoints_dirty = false;
        self.last_lite = Some(now);
        self.last_msg = Some(now);
    }

    /// The map stream ended with `reason`. True if the node is no longer
    /// registered (control said so) and must register again.
    pub fn map_ended(&mut self, reason: &str) -> bool {
        warn!("node: map stream ended: {reason}");
        let unregistered = reason.contains("HTTP 401")
            || reason.contains("HTTP 403")
            || reason.contains("not registered")
            || reason.contains("expired");
        if unregistered {
            self.registered = false;
            self.actions.push_back(Action::Store {
                key: KEY_REGISTERED,
                value: None,
            });
        } else {
            self.set_state(NodeState::Error(format!("map: {reason}")));
        }
        unregistered
    }

    /// True if control has said nothing (not even a keep-alive) for so
    /// long that the stream must be dead.
    pub fn map_silent(&self, now: Instant) -> bool {
        self.last_msg
            .is_some_and(|t| now.duration_since(t) > MAP_KEEPALIVE_TIMEOUT)
    }

    /// The node's endpoints changed; control is told soon.
    pub fn endpoints_changed(&mut self) {
        self.endpoints_dirty = true;
    }

    /// An endpoint update to send now, if one is due.
    pub fn lite_update(&mut self, magic: &MagicCore, now: Instant) -> Option<MapRequest> {
        let due = self.endpoints_dirty
            && self
                .last_lite
                .is_none_or(|t| now.duration_since(t) > LITE_UPDATE_INTERVAL);
        if !due {
            return None;
        }
        self.endpoints_dirty = false;
        self.last_lite = Some(now);
        let lite = self.map_request(false, magic);
        debug!(
            "node: endpoint update {:?} derp {}",
            lite.endpoints,
            magic.home_derp()
        );
        Some(lite)
    }

    /// The endpoint update could not be sent; it is tried again.
    pub fn lite_update_failed(&mut self) {
        self.endpoints_dirty = true;
    }

    /// Timers: chooses the home region once the STUN answers had time to
    /// come in. Call it at least every second or so.
    pub fn tick(&mut self, magic: &mut MagicCore, now: Instant) {
        if self.home_probe_due.is_some_and(|t| now >= t) {
            self.home_probe_due = None;
            let home = magic
                .nearest_region()
                .or_else(|| magic.fallback_region(&self.settings.fallback_region_code));
            if let Some(r) = home {
                info!("node: home DERP region {r}");
                magic.connect_home_derp(r, now);
                self.actions.push_back(Action::Store {
                    key: KEY_HOME_DERP,
                    value: Some(r.to_string()),
                });
                self.endpoints_dirty = true;
            }
        }
    }

    /// A message from the map stream.
    /// A peer handed over while a map message is being parsed (see
    /// [`crate::types::peer_stream`]); the rest of the message follows in
    /// [`NodeCore::on_map_response`].
    pub fn on_peer_event(&mut self, ev: PeerEvent, magic: &mut MagicCore) {
        match ev {
            PeerEvent::Node(list, node) => {
                if list == PeerList::Full {
                    if let Some(key) = node.key {
                        self.full_peers.push(key);
                    }
                }
                magic.peers_changed(alloc::vec![node]);
            }
            PeerEvent::End(PeerList::Full) => {
                let keys = core::mem::take(&mut self.full_peers);
                magic.retain_peers(&keys);
                info!("node: {} peers", keys.len());
            }
            PeerEvent::End(PeerList::Changed) => {}
        }
    }

    pub fn on_map_response(&mut self, mut resp: MapResponse, magic: &mut MagicCore, now: Instant) {
        self.last_msg = Some(now);
        debug!(
            "node: map message: keepalive={} node={} peers={:?} changed={:?} removed={:?} patch={:?} derpmap={}",
            resp.keep_alive,
            resp.node.is_some(),
            resp.peers.as_ref().map(|p| p.len()),
            resp.peers_changed.as_ref().map(|p| p.len()),
            resp.peers_removed.as_ref().map(|p| p.len()),
            resp.peers_changed_patch.as_ref().map(|p| p.len()),
            resp.derp_map.is_some()
        );
        if resp.keep_alive
            && resp.node.is_none()
            && resp.peers.is_none()
            && resp.peers_changed.is_none()
            && resp.peers_changed_patch.is_none()
        {
            return;
        }
        self.map_updates += 1;
        if let Some(ct) = resp.control_time.as_deref().and_then(parse_rfc3339_secs) {
            // Devices without a clock of their own date WireGuard handshakes
            // by control's time.
            magic.set_unix_time(now, Duration::from_secs(ct.max(0) as u64));
        }
        if let Some(node) = &resp.node {
            let mut addrs = Vec::new();
            let mut cidrs = Vec::new();
            for &(ip, _) in &node.addresses {
                let prefix = if ip.is_ipv4() { 10 } else { 48 };
                addrs.push(ip);
                cidrs.push((ip, prefix));
            }
            self.actions.push_back(Action::Addresses(cidrs));
            if self.addresses != addrs {
                info!("node: addresses {:?}", addrs);
            }
            self.addresses = addrs;
            self.dns_name = node.name.trim_end_matches('.').to_string();
        }
        if let Some(map) = resp.derp_map.take() {
            let regions = map.regions.len();
            magic.set_derp_map(map);
            debug!("node: DERP map with {regions} regions");
            let known = magic.home_derp();
            if known != 0 {
                // The home region was restored before the map arrived;
                // (re)connect now that the region's nodes are known.
                magic.connect_home_derp(known, now);
                self.endpoints_dirty = true;
            } else if self.home_probe_due.is_none() {
                magic.probe_derp_regions(now);
                self.home_probe_due = Some(now + HOME_PROBE_WAIT);
            }
        }
        if let Some(peers) = resp.peers.take() {
            let count = peers.len();
            magic.set_peers(peers);
            info!("node: {count} peers");
        }
        if let Some(changed) = resp.peers_changed.take() {
            magic.peers_changed(changed);
        }
        if let Some(removed) = &resp.peers_removed {
            magic.peers_removed(removed);
        }
        if let Some(patches) = &resp.peers_changed_patch {
            magic.apply_patches(patches);
        }
        // ACL: the legacy field replaces everything; the named sets are
        // merged (a null entry deletes that set).
        let mut filter_changed = false;
        if let Some(rules) = resp.packet_filter.take() {
            self.filter_sets.clear();
            self.filter_sets.insert("*".into(), rules);
            filter_changed = true;
        }
        if let Some(named) = resp.packet_filters.take() {
            for (name, rules) in named {
                match rules {
                    Some(r) => {
                        self.filter_sets.insert(name, r);
                    }
                    None => {
                        self.filter_sets.remove(&name);
                    }
                }
            }
            filter_changed = true;
        }
        if filter_changed {
            let all: Vec<FilterRule> = self.filter_sets.values().flatten().cloned().collect();
            debug!("node: packet filter with {} rules", all.len());
            magic.set_packet_filter(Filter::compile(&all));
        }
        if let Some(h) = &resp.health {
            for m in h {
                warn!("node: control health: {m}");
            }
        }
        if resp.ping_request.is_some() {
            debug!("node: ignoring control PingRequest");
        }
    }
}

/// Parses an RFC 3339 timestamp (as emitted by Go) into Unix seconds.
pub fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;
    // Time zone: 'Z' or +hh:mm / -hh:mm at the end.
    let mut tz = 0i64;
    if let Some(pos) = s.rfind(['+', '-']) {
        if pos > 18 {
            let sign = if &s[pos..pos + 1] == "+" { 1 } else { -1 };
            let h: i64 = s.get(pos + 1..pos + 3)?.parse().ok()?;
            let m: i64 = s.get(pos + 4..pos + 6)?.parse().ok()?;
            tz = sign * (h * 3600 + m * 60);
        }
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + min * 60 + sec - tz)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::WallClock;
    use alloc::boxed::Box;
    use alloc::vec;

    fn settings() -> NodeSettings {
        NodeSettings {
            hostname: "dev".into(),
            version: "1".into(),
            auth_key: Some("tskey-x".into()),
            fallback_region_code: "tok".into(),
        }
    }

    fn magic() -> MagicCore {
        MagicCore::new(
            KeyPair::generate(),
            KeyPair::generate(),
            41641,
            WallClock::default(),
        )
    }

    fn actions(n: &mut NodeCore) -> Vec<Action> {
        core::iter::from_fn(|| n.poll_action()).collect()
    }

    fn peer(id: i64, key: u8) -> Box<crate::types::Node> {
        Box::new(crate::types::Node {
            id,
            name: format!("p{id}"),
            key: Some([key; 32]),
            ..Default::default()
        })
    }

    #[test]
    fn a_full_list_one_peer_at_a_time_drops_the_missing_peers() {
        let mut m = magic();
        let mut n = NodeCore::new(settings(), KeyPair::generate(), true, None);
        let t = Instant::from_millis(0);
        m.set_peers(vec![peer(1, 1), peer(2, 2), peer(3, 3)]);
        n.starting_map(&mut m, t);
        for p in [peer(1, 1), peer(3, 3), peer(4, 4)] {
            n.on_peer_event(PeerEvent::Node(PeerList::Full, p), &mut m);
        }
        // Until the list ends, nobody is dropped.
        assert_eq!(m.peer_status(t).len(), 4);
        n.on_peer_event(PeerEvent::End(PeerList::Full), &mut m);
        let mut names: Vec<String> = m.peer_status(t).into_iter().map(|p| p.name).collect();
        names.sort();
        assert_eq!(names, ["p1", "p3", "p4"]);
        // A changed list adds and keeps the rest.
        n.on_peer_event(PeerEvent::Node(PeerList::Changed, peer(5, 5)), &mut m);
        n.on_peer_event(PeerEvent::End(PeerList::Changed), &mut m);
        assert_eq!(m.peer_status(t).len(), 4);
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_secs("2026-09-07T09:00:00Z"), Some(1788771600));
        assert_eq!(
            parse_rfc3339_secs("2026-09-07T18:00:00.123456+09:00"),
            Some(1788771600)
        );
    }

    #[test]
    fn an_expired_node_key_is_replaced_and_registered() {
        let mut m = magic();
        let first = KeyPair::generate();
        let mut n = NodeCore::new(settings(), first.clone(), false, None);
        assert!(n.needs_registration());
        let req = n.register_request();
        assert_eq!(req.node_key, fmt_node_pub(first.public()));
        assert_eq!(req.auth.unwrap().auth_key, "tskey-x");
        let expired = RegisterResponse {
            node_key_expired: true,
            ..Default::default()
        };
        assert_eq!(
            n.on_register_response(expired, &mut m),
            Ok(Registration::Again)
        );
        // The second request names the old key and a new one.
        let req = n.register_request();
        assert_eq!(req.old_node_key, fmt_node_pub(first.public()));
        assert_ne!(req.node_key, fmt_node_pub(first.public()));
        let ok = RegisterResponse {
            machine_authorized: true,
            ..Default::default()
        };
        assert_eq!(
            n.on_register_response(ok, &mut m),
            Ok(Registration::Authorized)
        );
        assert_eq!(fmt_node_pub(n.node_key().public()), req.node_key);
        assert_eq!(m.node_key().public(), n.node_key().public());
        assert!(!n.needs_registration());
        let acts = actions(&mut n);
        assert!(matches!(
            &acts[0],
            Action::Store {
                key: KEY_NODE,
                value: Some(_)
            }
        ));
        assert_eq!(
            acts[1],
            Action::Store {
                key: KEY_REGISTERED,
                value: Some("1".into())
            }
        );
    }

    #[test]
    fn approval_by_url_and_losing_registration() {
        let mut m = magic();
        let mut n = NodeCore::new(settings(), KeyPair::generate(), false, None);
        n.register_request();
        let wait = RegisterResponse {
            auth_url: "https://login.example/a/1".into(),
            ..Default::default()
        };
        assert_eq!(
            n.on_register_response(wait, &mut m),
            Ok(Registration::Waiting)
        );
        assert_eq!(
            n.state(),
            &NodeState::NeedsLogin("https://login.example/a/1".into())
        );
        let denied = RegisterResponse {
            error: "invalid key".into(),
            ..Default::default()
        };
        assert_eq!(
            n.on_register_response(denied, &mut m),
            Err("invalid key".into())
        );
        // A registered node that control no longer knows registers again.
        let mut n = NodeCore::new(settings(), KeyPair::generate(), true, None);
        assert!(!n.map_ended("network unreachable"));
        assert!(!n.needs_registration());
        assert!(n.map_ended("map: HTTP 401: node not found"));
        assert!(n.needs_registration());
        assert_eq!(
            actions(&mut n),
            [Action::Store {
                key: KEY_REGISTERED,
                value: None
            }]
        );
    }

    #[test]
    fn the_first_map_sets_addresses_peers_filter_and_home() {
        let mut m = magic();
        let mut n = NodeCore::new(settings(), KeyPair::generate(), true, None);
        let t0 = Instant::from_millis(5000);
        n.map_connected(t0);
        let mut map = DerpMap::default();
        map.regions.insert(
            7,
            DerpRegion {
                region_id: 7,
                region_code: "tok".into(),
                nodes: vec![DerpNode {
                    region_id: 7,
                    host_name: "derp7".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let resp = MapResponse {
            node: Some(crate::types::Node {
                name: "dev.example.ts.net.".into(),
                addresses: vec![("100.64.0.9".parse().unwrap(), 32)],
                ..Default::default()
            }),
            peers: Some(vec![Box::new(crate::types::Node {
                id: 2,
                key: Some([2; 32]),
                ..Default::default()
            })]),
            derp_map: Some(map),
            packet_filter: Some(vec![]),
            control_time: Some("2026-11-19T00:00:00Z".into()),
            ..Default::default()
        };
        n.on_map_response(resp, &mut m, t0);
        let st = n.status(&m, t0);
        assert_eq!(st.dns_name, "dev.example.ts.net");
        assert_eq!(st.peers.len(), 1);
        assert_eq!(st.map_updates, 1);
        assert_eq!(
            actions(&mut n),
            [Action::Addresses(vec![("100.64.0.9".parse().unwrap(), 10)])]
        );
        assert_eq!(
            m.wall_clock().unix(t0),
            Duration::from_secs(parse_rfc3339_secs("2026-11-19T00:00:00Z").unwrap() as u64)
        );
        // No STUN answers: after the wait, the fallback region by code.
        n.tick(&mut m, t0 + Duration::from_millis(1000));
        assert_eq!(m.home_derp(), 0);
        n.tick(&mut m, t0 + Duration::from_millis(1600));
        assert_eq!(m.home_derp(), 7);
        assert_eq!(
            actions(&mut n),
            [Action::Store {
                key: KEY_HOME_DERP,
                value: Some("7".into())
            }]
        );
        // Then control hears about the endpoints, at most every 3 s.
        let later = t0 + Duration::from_secs(4);
        let lite = n.lite_update(&m, later).unwrap();
        assert!(!lite.stream && lite.omit_peers);
        assert!(n.lite_update(&m, later).is_none());
        assert!(!n.map_silent(later));
        assert!(n.map_silent(t0 + Duration::from_secs(151)));
    }
}
