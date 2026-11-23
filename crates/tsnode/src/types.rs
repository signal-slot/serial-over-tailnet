//! JSON types exchanged with the Tailscale control plane (subset of `tailcfg`).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use serde::{Deserialize, Serialize};

/// The capability version we claim. Kept deliberately conservative so the
/// control server uses the well-understood, older response shapes.
pub const CAPABILITY_VERSION: u16 = 106;

pub const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct NetInfo {
    #[serde(rename = "PreferredDERP")]
    pub preferred_derp: i32,
    #[serde(rename = "WorkingUDP")]
    pub working_udp: Option<bool>,
    #[serde(rename = "WorkingIPv6")]
    pub working_ipv6: Option<bool>,
    pub link_type: String,
    #[serde(rename = "DERPLatency", skip_serializing_if = "BTreeMap::is_empty")]
    pub derp_latency: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct Hostinfo {
    #[serde(rename = "IPNVersion")]
    pub ipn_version: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "OSVersion")]
    pub os_version: String,
    pub distro: String,
    pub device_model: String,
    pub hostname: String,
    pub go_arch: String,
    pub machine: String,
    pub userspace: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_info: Option<NetInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct RegisterResponseAuth {
    pub auth_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct RegisterRequest {
    pub version: u16,
    pub node_key: String,
    pub old_node_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<RegisterResponseAuth>,
    pub expiry: String,
    pub followup: String,
    pub hostinfo: Hostinfo,
    #[serde(skip_serializing_if = "core::ops::Not::not")]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserProfile {
    #[serde(rename = "ID")]
    pub id: i64,
    pub login_name: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct Login {
    #[serde(rename = "ID")]
    pub id: i64,
    pub login_name: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct RegisterResponse {
    pub login: Login,
    pub node_key_expired: bool,
    pub machine_authorized: bool,
    #[serde(rename = "AuthURL")]
    pub auth_url: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct MapRequest {
    pub version: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub compress: String,
    pub keep_alive: bool,
    pub node_key: String,
    pub disco_key: String,
    pub stream: bool,
    pub hostinfo: Hostinfo,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<String>,
    #[serde(skip_serializing_if = "core::ops::Not::not")]
    pub omit_peers: bool,
    #[serde(skip_serializing_if = "core::ops::Not::not")]
    pub read_only: bool,
}

/// A node of the tailnet as the network map describes it.
///
/// Only what the node uses is declared, and keys and addresses are parsed
/// while the message is read: the parser skips the other fields without
/// storing them, and a key takes 32 bytes instead of a 72-character string.
/// That matters on a device without PSRAM: the first map of a tailnet of 48
/// peers (69 KB of JSON) did not fit an ESP32-C3 with every field kept as
/// text.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct Node {
    #[serde(rename = "ID")]
    pub id: i64,
    pub name: String,
    /// `None` if absent or not a valid `nodekey:`.
    #[serde(deserialize_with = "de::node_key")]
    pub key: Option<[u8; 32]>,
    #[serde(deserialize_with = "de::disco_key")]
    pub disco_key: Option<[u8; 32]>,
    /// The node's own addresses, with their prefix lengths.
    #[serde(deserialize_with = "de::cidrs")]
    pub addresses: Vec<(IpAddr, u8)>,
    #[serde(rename = "AllowedIPs", deserialize_with = "de::cidrs")]
    pub allowed_ips: Vec<(IpAddr, u8)>,
    #[serde(deserialize_with = "de::socket_addrs")]
    pub endpoints: Vec<SocketAddr>,
    /// Legacy home DERP encoding `127.3.3.40:<region>`, as the region; 0 if
    /// absent.
    #[serde(rename = "DERP", deserialize_with = "de::derp_region")]
    pub derp: i32,
    #[serde(rename = "HomeDERP")]
    pub home_derp: i32,
    pub machine_authorized: bool,
}

impl Node {
    pub fn home_derp_region(&self) -> Option<i32> {
        [self.home_derp, self.derp].into_iter().find(|r| *r != 0)
    }
}

/// `addr/len`, or a bare address (a host route).
pub(crate) fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (ip, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip.parse::<IpAddr>().ok()?, p.parse::<u8>().ok()?),
        None => {
            let ip = s.parse::<IpAddr>().ok()?;
            (ip, if ip.is_ipv4() { 32 } else { 128 })
        }
    };
    Some((ip, prefix))
}

/// Parsers for the fields of [`Node`] that are kept in binary form. A value
/// that does not parse is dropped, as the code that used the text did; a
/// `null` is taken as absent.
mod de {
    use crate::keys::{parse_prefixed, DISCO_PUB_PREFIX, NODE_PUB_PREFIX};
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::net::{IpAddr, Ipv4Addr, SocketAddr};
    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer};

    fn text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
    }

    fn list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
    }

    pub fn node_key<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        Ok(parse_prefixed(NODE_PUB_PREFIX, &text(d)?))
    }

    pub fn disco_key<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        Ok(parse_prefixed(DISCO_PUB_PREFIX, &text(d)?))
    }

    pub fn cidrs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(IpAddr, u8)>, D::Error> {
        Ok(list(d)?
            .iter()
            .filter_map(|c| super::parse_cidr(c))
            .collect())
    }

    pub fn socket_addrs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SocketAddr>, D::Error> {
        Ok(list(d)?.iter().filter_map(|e| e.parse().ok()).collect())
    }

    pub fn ipv4<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Ipv4Addr>, D::Error> {
        Ok(text(d)?.parse().ok())
    }

    pub fn derp_nodes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<super::DerpNode>, D::Error> {
        struct Keep;
        impl<'de> Visitor<'de> for Keep {
            type Value = Vec<super::DerpNode>;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.write_str("a list of DERP nodes")
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut kept = Vec::new();
                let (mut have_relay, mut have_stun) = (false, false);
                while let Some(n) = seq.next_element::<super::DerpNode>()? {
                    let relay = !n.stun_only && !have_relay;
                    let stun = n.ipv4.is_some() && !have_stun;
                    if relay || stun {
                        have_relay |= relay;
                        have_stun |= stun;
                        kept.push(n);
                    }
                }
                kept.shrink_to_fit();
                Ok(kept)
            }
        }
        d.deserialize_any(Keep)
    }

    pub fn derp_region<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
        Ok(text(d)?
            .rsplit(':')
            .next()
            .and_then(|r| r.parse().ok())
            .unwrap_or(0))
    }

    // Boxed on purpose, as in `MapResponse::peers`.
    #[allow(clippy::vec_box)]
    pub fn peers_full<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<Box<super::Node>>>, D::Error> {
        peer_list(d, super::PeerList::Full)
    }

    #[allow(clippy::vec_box)]
    pub fn peers_changed<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<Box<super::Node>>>, D::Error> {
        peer_list(d, super::PeerList::Changed)
    }

    /// A list of nodes, or `None` for `null`. While a peer sink is set
    /// (see [`super::peer_stream`]) each node goes to it as soon as it is
    /// parsed, and the list comes back as `None`.
    #[allow(clippy::vec_box)]
    fn peer_list<'de, D: Deserializer<'de>>(
        d: D,
        kind: super::PeerList,
    ) -> Result<Option<Vec<Box<super::Node>>>, D::Error> {
        struct Nodes(super::PeerList);
        impl<'de> Visitor<'de> for Nodes {
            type Value = Option<Vec<Box<super::Node>>>;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.write_str("a list of nodes or null")
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_seq(self)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut nodes = Vec::new();
                let mut streamed = false;
                while let Some(n) = seq.next_element::<Box<super::Node>>()? {
                    match super::peer_stream::offer(super::PeerEvent::Node(self.0, n)) {
                        Some(super::PeerEvent::Node(_, n)) => nodes.push(n),
                        _ => streamed = true,
                    }
                }
                // Sent even after streamed nodes: the end of a full list is
                // when the peers missing from it are dropped.
                let end = super::peer_stream::offer(super::PeerEvent::End(self.0));
                if streamed || end.is_none() {
                    return Ok(None);
                }
                Ok(Some(nodes))
            }
        }
        d.deserialize_option(Nodes(kind))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct PeerChange {
    #[serde(rename = "NodeID")]
    pub node_id: i64,
    #[serde(rename = "DERPRegion")]
    pub derp_region: i32,
    pub endpoints: Option<Vec<String>>,
    pub key: Option<String>,
    pub disco_key: Option<String>,
}

/// A DERP server as the map gives it; only what dialing and STUN use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct DerpNode {
    #[serde(rename = "RegionID")]
    pub region_id: i32,
    pub host_name: String,
    #[serde(rename = "IPv4", deserialize_with = "de::ipv4")]
    pub ipv4: Option<Ipv4Addr>,
    #[serde(rename = "STUNPort")]
    pub stun_port: i32,
    #[serde(rename = "STUNOnly")]
    pub stun_only: bool,
    #[serde(rename = "DERPPort")]
    pub derp_port: i32,
    pub insecure_for_tests: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct DerpRegion {
    #[serde(rename = "RegionID")]
    pub region_id: i32,
    pub region_code: String,
    pub avoid: bool,
    /// The nodes that are ever used: the first that relays (`STUNOnly`
    /// false), which is the one dialed, and the first with an IPv4 address,
    /// which is the one probed with STUN; one node if it is both. A region
    /// lists several, and there are some thirty regions.
    #[serde(deserialize_with = "de::derp_nodes")]
    pub nodes: Vec<DerpNode>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct DerpMap {
    /// By region ID.
    pub regions: BTreeMap<i32, DerpRegion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct DnsConfig {
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct PingRequest {
    #[serde(rename = "URL")]
    pub url: String,
    pub log: bool,
    pub types: String,
    #[serde(rename = "IP")]
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, rename_all = "PascalCase")]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, rename_all = "PascalCase")]
pub struct NetPortRange {
    #[serde(rename = "IP")]
    pub ip: String,
    pub ports: PortRange,
}

/// One ACL rule as sent by control (`tailcfg.FilterRule`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, rename_all = "PascalCase")]
pub struct FilterRule {
    #[serde(rename = "SrcIPs")]
    pub src_ips: Vec<String>,
    pub dst_ports: Vec<NetPortRange>,
    #[serde(rename = "IPProto")]
    pub ip_proto: Vec<i32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
pub struct MapResponse {
    pub seq: i64,
    pub keep_alive: bool,
    pub ping_request: Option<PingRequest>,
    pub node: Option<Node>,
    #[serde(rename = "DERPMap")]
    pub derp_map: Option<DerpMap>,
    /// Each node boxed: a list of 50 in one block (about 9 KB on a 32-bit
    /// chip, doubling as it grows) was more than an ESP32-C3 had free in
    /// one piece while the map was read.
    ///
    /// Read with a peer sink set (see [`peer_stream`]), both lists come
    /// back as `None`: their nodes went to the sink one at a time.
    #[serde(deserialize_with = "de::peers_full")]
    pub peers: Option<Vec<Box<Node>>>,
    #[serde(deserialize_with = "de::peers_changed")]
    pub peers_changed: Option<Vec<Box<Node>>>,
    pub peers_removed: Option<Vec<i64>>,
    pub peers_changed_patch: Option<Vec<PeerChange>>,
    /// Legacy whole-filter replacement. `Some(vec![])` denies everything.
    pub packet_filter: Option<Vec<FilterRule>>,
    /// Named filter sets (capability version 81+): `*` is the base set, a
    /// `null` value deletes the named set.
    pub packet_filters: Option<BTreeMap<String, Option<Vec<FilterRule>>>>,
    pub domain: String,
    pub health: Option<Vec<String>>,
    pub control_time: Option<String>,
}

/// The two peer lists of a [`MapResponse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerList {
    /// `Peers`: every peer; the ones not in it are gone.
    Full,
    /// `PeersChanged`: peers added or changed.
    Changed,
}

/// What a peer sink receives while a map message is parsed.
#[derive(Debug)]
pub enum PeerEvent {
    /// The next node of a list.
    Node(PeerList, Box<Node>),
    /// The list is complete (also sent for an empty one, not for `null`).
    End(PeerList),
}

/// Peers handed over one at a time while a map message is parsed, instead
/// of collected in it.
///
/// On a reconnect the first message repeats every peer, and parsed as a
/// whole it was held next to the peer table it replaces: about 30 KB more
/// than an ESP32-C3 has free, which restarted it each time the map stream
/// reconnected. With a sink, one node at a time is held.
#[cfg(feature = "std")]
pub mod peer_stream {
    use super::PeerEvent;
    use std::cell::Cell;

    type Sink = dyn FnMut(PeerEvent);

    std::thread_local! {
        static SINK: Cell<Option<*mut Sink>> = const { Cell::new(None) };
    }

    /// Runs `f`; map messages parsed by it on this thread hand their peers
    /// to `sink` as they are read.
    pub fn with_sink<R>(sink: &mut dyn FnMut(PeerEvent), f: impl FnOnce() -> R) -> R {
        struct Restore(Option<*mut Sink>);
        impl Drop for Restore {
            fn drop(&mut self) {
                SINK.with(|s| s.set(self.0));
            }
        }
        // SAFETY: only the lifetime is erased. The pointer is taken out
        // again when this function returns or unwinds (`Restore`), while
        // `sink` is still borrowed, and only this thread sees it.
        let ptr: *mut Sink = unsafe { core::mem::transmute(sink) };
        let _restore = Restore(SINK.with(|s| s.replace(Some(ptr))));
        f()
    }

    /// Gives `ev` to the sink, if one is set; otherwise returns it.
    pub(crate) fn offer(ev: PeerEvent) -> Option<PeerEvent> {
        match SINK.with(|s| s.get()) {
            Some(sink) => {
                // SAFETY: see `with_sink`. The sink does not parse maps, so
                // it is not entered twice.
                unsafe { (*sink)(ev) };
                None
            }
            None => Some(ev),
        }
    }
}

/// Without std there is no sink: peers are always collected.
#[cfg(not(feature = "std"))]
pub mod peer_stream {
    use super::PeerEvent;

    pub(crate) fn offer(ev: PeerEvent) -> Option<PeerEvent> {
        Some(ev)
    }
}

/// Builds the `Hostinfo` we advertise.
pub fn hostinfo(hostname: &str, version: &str, preferred_derp: i32) -> Hostinfo {
    Hostinfo {
        ipn_version: format!("tsnode-{version}"),
        os: "linux".into(),
        os_version: "esp-idf".into(),
        distro: "esp-idf".into(),
        device_model: "ESP32-S3".into(),
        hostname: hostname.into(),
        go_arch: "xtensa".into(),
        machine: "esp32s3".into(),
        userspace: true,
        net_info: if preferred_derp != 0 {
            Some(NetInfo {
                preferred_derp,
                working_udp: Some(true),
                link_type: "wifi".into(),
                ..Default::default()
            })
        } else {
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_json(id: i64, key: u8) -> String {
        format!(
            r#"{{"ID":{id},"Key":"nodekey:{}","Name":"p{id}"}}"#,
            format!("{key:02x}").repeat(32)
        )
    }

    #[test]
    fn peer_lists_are_collected_without_a_sink() {
        let json = format!(
            r#"{{"Peers":[{},{}],"PeersChanged":[],"PeersRemoved":[5]}}"#,
            peer_json(1, 1),
            peer_json(2, 2)
        );
        let r: MapResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(r.peers.unwrap().len(), 2);
        assert_eq!(r.peers_changed.unwrap().len(), 0);
        let r: MapResponse = serde_json::from_str(r#"{"Peers":null}"#).unwrap();
        assert!(r.peers.is_none() && r.peers_changed.is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn a_sink_gets_peers_one_at_a_time() {
        let json = format!(
            r#"{{"Peers":[{},{}],"PeersChanged":[],"DERPMap":null,"PeersRemoved":[5]}}"#,
            peer_json(1, 1),
            peer_json(2, 2)
        );
        let mut seen = Vec::new();
        let mut sink = |ev: PeerEvent| {
            seen.push(match ev {
                PeerEvent::Node(l, n) => format!("{l:?} {}", n.id),
                PeerEvent::End(l) => format!("{l:?} end"),
            })
        };
        let r: MapResponse =
            peer_stream::with_sink(&mut sink, || serde_json::from_str(&json)).unwrap();
        assert_eq!(seen, ["Full 1", "Full 2", "Full end", "Changed end"]);
        assert!(r.peers.is_none() && r.peers_changed.is_none());
        assert_eq!(r.peers_removed, Some(vec![5]));
        // The sink is gone afterwards.
        let r: MapResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(r.peers.unwrap().len(), 2);
    }

    #[test]
    fn node_keys_and_addresses_are_parsed_and_the_rest_skipped() {
        let key = "1".repeat(64);
        let json = format!(
            r#"{{"ID":5,"StableID":"n5","Name":"pi.example.ts.net.","User":3,
                "Key":"nodekey:{key}","KeyExpiry":"2027-01-01T00:00:00Z",
                "DiscoKey":"discokey:{key}","Machine":"mkey:00",
                "Addresses":["100.64.0.9/32","fd7a:115c:a1e0::9/128"],
                "AllowedIPs":["100.64.0.9/32","192.168.1.0/24","bad"],
                "Endpoints":["198.51.100.7:41641","[2001:db8::7]:41641","nonsense"],
                "DERP":"127.3.3.40:9","Hostinfo":{{"OS":"linux","Services":[{{"Port":22}}]}},
                "Online":true,"MachineAuthorized":true}}"#
        );
        let n: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(n.key, Some([0x11; 32]));
        assert_eq!(n.disco_key, Some([0x11; 32]));
        assert_eq!(n.addresses[0], ("100.64.0.9".parse().unwrap(), 32));
        assert_eq!(n.addresses[1].1, 128);
        assert_eq!(n.allowed_ips.len(), 2);
        assert_eq!(n.allowed_ips[1], ("192.168.1.0".parse().unwrap(), 24));
        assert_eq!(n.endpoints.len(), 2);
        assert_eq!(n.endpoints[1].port(), 41641);
        assert_eq!(n.home_derp_region(), Some(9));
        assert!(n.machine_authorized);
        // A null where a value is expected, and a key that is not one.
        let n: Node =
            serde_json::from_str(r#"{"Key":"nodekey:zz","DiscoKey":null,"Endpoints":null}"#)
                .unwrap();
        assert_eq!((n.key, n.disco_key, n.endpoints.len()), (None, None, 0));
    }

    #[test]
    fn a_derp_region_keeps_the_relay_and_the_stun_node_only() {
        let m: DerpMap = serde_json::from_str(
            r#"{"Regions":{"7":{"RegionID":7,"RegionCode":"tok","Nodes":[
                {"Name":"7s","HostName":"s.example","STUNOnly":true},
                {"Name":"7a","HostName":"a.example","IPv4":"203.0.113.1","CertName":"x"},
                {"Name":"7b","HostName":"b.example","IPv4":"203.0.113.2"}]},
              "9":{"RegionID":9,"Nodes":[
                {"HostName":"c.example"},
                {"HostName":"d.example","IPv4":"203.0.113.4","STUNOnly":true},
                {"HostName":"e.example","IPv4":"203.0.113.5"}]},
              "11":{"RegionID":11,"Nodes":null}}}"#,
        )
        .unwrap();
        let hosts = |r: i32| -> Vec<&str> {
            m.regions[&r]
                .nodes
                .iter()
                .map(|n| n.host_name.as_str())
                .collect()
        };
        // 7: the first relay also has the first IPv4.
        assert_eq!(hosts(7), ["a.example"]);
        assert_eq!(
            m.regions[&7].nodes[0].ipv4,
            Some("203.0.113.1".parse().unwrap())
        );
        // 9: the relay has no address; the STUN node is a later one.
        assert_eq!(hosts(9), ["c.example", "d.example"]);
        assert!(hosts(11).is_empty());
    }

    #[test]
    fn node_derp_parsing() {
        let n: Node = serde_json::from_str(
            r#"{"ID":1,"Key":"nodekey:00","DERP":"127.3.3.40:4","Addresses":["100.64.0.1/32"]}"#,
        )
        .unwrap();
        assert_eq!(n.home_derp_region(), Some(4));
        let n: Node = serde_json::from_str(r#"{"HomeDERP":7}"#).unwrap();
        assert_eq!(n.home_derp_region(), Some(7));
    }

    #[test]
    fn packet_filter_parsing() {
        let m: MapResponse = serde_json::from_str(
            r#"{"PacketFilter":[{"SrcIPs":["*"],"DstPorts":[{"IP":"*","Ports":{"First":0,"Last":65535}}],"IPProto":[6,17]}],
                "PacketFilters":{"*":[{"SrcIPs":["100.64.0.1"],"DstPorts":[{"IP":"100.64.0.2/32","Ports":{"First":22,"Last":22}}]}],"old":null}}"#,
        )
        .unwrap();
        let pf = m.packet_filter.unwrap();
        assert_eq!(pf[0].src_ips, vec!["*"]);
        assert_eq!(pf[0].dst_ports[0].ports.last, 65535);
        assert_eq!(pf[0].ip_proto, vec![6, 17]);
        let pfs = m.packet_filters.unwrap();
        assert_eq!(pfs["*"].as_ref().unwrap()[0].dst_ports[0].ports.first, 22);
        assert!(pfs["old"].is_none());
        let m: MapResponse = serde_json::from_str(r#"{"PacketFilter":[]}"#).unwrap();
        assert_eq!(m.packet_filter.unwrap().len(), 0);
        let m: MapResponse = serde_json::from_str(r#"{"KeepAlive":true}"#).unwrap();
        assert!(m.packet_filter.is_none() && m.packet_filters.is_none());
    }

    #[test]
    fn register_request_json_shape() {
        let r = RegisterRequest {
            version: 106,
            node_key: "nodekey:ab".into(),
            expiry: ZERO_TIME.into(),
            ..Default::default()
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["Version"], 106);
        assert_eq!(j["NodeKey"], "nodekey:ab");
        assert!(j.get("Auth").is_none());
        assert!(j.get("Ephemeral").is_none());
        let h = serde_json::to_value(Hostinfo {
            os: "linux".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(h["OS"], "linux");
        assert!(h.get("IPNVersion").is_some());
    }
}
