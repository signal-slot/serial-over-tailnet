//! A tiny userspace TCP/IP stack on top of smoltcp that terminates TCP on the
//! node's tailnet addresses. IP packets flow in from WireGuard and out to it.
//!
//! [`NetCore`] is the stack itself and does no I/O. The caller hands it the
//! IP packets that arrived ([`NetCore::receive`]) and the time
//! ([`NetCore::poll`]), moves bytes in and out of its TCP connections, then
//! takes out the packets to send ([`NetCore::poll_transmit`]) and asks when
//! to poll again ([`NetCore::poll_delay`]).
//!
//! [`NetStack`] (`std`) runs it on a thread and offers blocking
//! [`TcpListener`] and [`TcpStream`] handles.

#[cfg(feature = "std")]
mod driver;
#[cfg(feature = "std")]
pub use driver::{NetStack, TcpListener, TcpStream};

use crate::time::Instant;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;
use alloc::vec::Vec;
use core::net::{IpAddr, SocketAddr};
use core::time::Duration;
use log::{debug, warn};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};

// Per socket (a listener holds one ready), so they count on a small device.
const RX_BUF: usize = if cfg!(feature = "small-ram") {
    2 * 1024
} else {
    8 * 1024
};
const TX_BUF: usize = if cfg!(feature = "small-ram") {
    2 * 1024
} else {
    8 * 1024
};
/// A peer that stops acknowledging (sleeping laptop, vanished NAT mapping)
/// is dropped after this long so the bridge becomes free again.
const DEAD_PEER_TIMEOUT: Duration = Duration::from_secs(120);
const MTU: usize = 1280;
/// Received packets waiting for the next poll; more are dropped.
const MAX_INBOUND: usize = 256;

/// Packets between the stack and the WireGuard side.
struct TunDevice {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct RxTok(Vec<u8>);
struct TxTok<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for RxTok {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl phy::TxToken for TxTok<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for TunDevice {
    type RxToken<'a> = RxTok;
    type TxToken<'a> = TxTok<'a>;

    fn receive(
        &mut self,
        _t: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let pkt = self.rx.pop_front()?;
        Some((RxTok(pkt), TxTok(&mut self.tx)))
    }
    fn transmit(&mut self, _t: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(TxTok(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

/// A listening port, from [`NetCore::listen`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListenerId(u32);

/// A TCP connection, accepted or opened. It stays valid until
/// [`NetCore::remove`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConnId(SocketHandle);

/// Where a connection is, as far as its user cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnStatus {
    /// The handshake has not finished.
    pub connecting: bool,
    /// More data can still arrive (the peer has not closed its side).
    pub may_recv: bool,
    /// The connection is over; [`NetCore::remove`] it.
    pub closed: bool,
}

struct Listener {
    port: u16,
    /// The socket waiting for the next connection.
    handle: SocketHandle,
    /// Connections that arrived and were not yet taken by `accept`.
    accepted: VecDeque<(ConnId, SocketAddr)>,
}

pub struct NetCore {
    device: TunDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    listeners: BTreeMap<ListenerId, Listener>,
    next_listener: u32,
}

fn smol_instant(now: Instant) -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_micros(now.as_micros() as i64)
}

fn to_smol(ip: IpAddr) -> IpAddress {
    match ip {
        IpAddr::V4(v4) => IpAddress::Ipv4(v4),
        IpAddr::V6(v6) => IpAddress::Ipv6(v6),
    }
}

fn from_smol(ep: IpEndpoint) -> SocketAddr {
    let ip = match ep.addr {
        IpAddress::Ipv4(a) => IpAddr::V4(a),
        IpAddress::Ipv6(a) => IpAddr::V6(a),
    };
    SocketAddr::new(ip, ep.port)
}

fn new_tcp_socket() -> tcp::Socket<'static> {
    let mut s = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; RX_BUF]),
        tcp::SocketBuffer::new(vec![0; TX_BUF]),
    );
    s.set_nagle_enabled(false);
    s.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
    s.set_timeout(Some(smoltcp::time::Duration::from_secs(
        DEAD_PEER_TIMEOUT.as_secs(),
    )));
    s
}

fn listening_socket(port: u16) -> Option<tcp::Socket<'static>> {
    let mut s = new_tcp_socket();
    match s.listen(IpListenEndpoint { addr: None, port }) {
        Ok(()) => Some(s),
        Err(e) => {
            warn!("netstack: listen on {port} failed: {e:?}");
            None
        }
    }
}

impl NetCore {
    /// A stack with no addresses. `random_seed` seeds smoltcp's choices
    /// (initial sequence numbers).
    pub fn new(random_seed: u64, now: Instant) -> Self {
        let mut device = TunDevice {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
        };
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = random_seed;
        let iface = Interface::new(config, &mut device, smol_instant(now));
        NetCore {
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            listeners: BTreeMap::new(),
            next_listener: 0,
        }
    }

    /// Sets the node's tailnet addresses (with their on-link prefixes).
    pub fn set_addresses(&mut self, addrs: &[(IpAddr, u8)]) {
        let cidrs: Vec<IpCidr> = addrs
            .iter()
            .map(|&(ip, prefix)| IpCidr::new(to_smol(ip), prefix))
            .collect();
        self.iface.update_ip_addrs(|a| {
            a.clear();
            for cidr in cidrs.iter().take(a.capacity()) {
                let _ = a.push(*cidr);
            }
        });
        for cidr in &cidrs {
            match cidr.address() {
                IpAddress::Ipv4(v4) => {
                    let _ = self.iface.routes_mut().add_default_ipv4_route(v4);
                }
                IpAddress::Ipv6(v6) => {
                    let _ = self.iface.routes_mut().add_default_ipv6_route(v6);
                }
            }
        }
        debug!("netstack: addresses {:?}", cidrs);
    }

    /// Takes an IP packet received from a peer; it is processed at the next
    /// [`NetCore::poll`]. Returns false (and drops it) when too many wait.
    pub fn receive(&mut self, pkt: Vec<u8>) -> bool {
        if self.device.rx.len() >= MAX_INBOUND {
            return false;
        }
        self.device.rx.push_back(pkt);
        true
    }

    /// Starts accepting connections on `port`.
    pub fn listen(&mut self, port: u16) -> Option<ListenerId> {
        let s = listening_socket(port)?;
        let id = ListenerId(self.next_listener);
        self.next_listener = self.next_listener.wrapping_add(1);
        let handle = self.sockets.add(s);
        self.listeners.insert(
            id,
            Listener {
                port,
                handle,
                accepted: VecDeque::new(),
            },
        );
        Some(id)
    }

    /// Stops listening; connections not yet accepted are reset.
    pub fn unlisten(&mut self, id: ListenerId) {
        let Some(l) = self.listeners.remove(&id) else {
            return;
        };
        let handles = core::iter::once(l.handle).chain(l.accepted.iter().map(|(c, _)| c.0));
        for h in handles {
            self.sockets.get_mut::<tcp::Socket>(h).abort();
            self.sockets.remove(h);
        }
    }

    /// The next connection that arrived on the listener, with its peer.
    pub fn accept(&mut self, id: ListenerId) -> Option<(ConnId, SocketAddr)> {
        self.listeners.get_mut(&id)?.accepted.pop_front()
    }

    /// Opens a connection to `remote` from `local_port`.
    pub fn connect(&mut self, remote: SocketAddr, local_port: u16) -> Option<ConnId> {
        let mut s = new_tcp_socket();
        let ep = IpEndpoint::new(to_smol(remote.ip()), remote.port());
        match s.connect(self.iface.context(), ep, local_port) {
            Ok(()) => Some(ConnId(self.sockets.add(s))),
            Err(e) => {
                warn!("netstack: connect to {remote} failed: {e:?}");
                None
            }
        }
    }

    fn sock(&mut self, id: ConnId) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(id.0)
    }

    pub fn can_recv(&mut self, id: ConnId) -> bool {
        self.sock(id).can_recv()
    }

    /// Copies received bytes into `buf`; 0 when there are none.
    pub fn recv(&mut self, id: ConnId, buf: &mut [u8]) -> usize {
        self.sock(id).recv_slice(buf).unwrap_or(0)
    }

    pub fn can_send(&mut self, id: ConnId) -> bool {
        self.sock(id).can_send()
    }

    /// Queues as much of `data` as fits; returns how much did.
    pub fn send(&mut self, id: ConnId, data: &[u8]) -> usize {
        self.sock(id).send_slice(data).unwrap_or(0)
    }

    /// Closes our side once what was queued has been sent.
    pub fn close(&mut self, id: ConnId) {
        let s = self.sock(id);
        if s.is_open() {
            s.close();
        }
    }

    /// Resets the connection.
    pub fn abort(&mut self, id: ConnId) {
        self.sock(id).abort();
    }

    pub fn status(&mut self, id: ConnId) -> ConnStatus {
        let s = self.sock(id);
        ConnStatus {
            connecting: matches!(s.state(), tcp::State::SynSent | tcp::State::SynReceived),
            may_recv: s.may_recv(),
            closed: matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait),
        }
    }

    /// Forgets a connection; its id must not be used again.
    pub fn remove(&mut self, id: ConnId) {
        self.sockets.remove(id.0);
    }

    /// Processes received packets and timers, and moves new connections to
    /// their listeners' accept queues.
    pub fn poll(&mut self, now: Instant) {
        let _ = self
            .iface
            .poll(smol_instant(now), &mut self.device, &mut self.sockets);

        // A listening socket that left LISTEN holds a connection: hand it
        // over and listen on a fresh one.
        for l in self.listeners.values_mut() {
            let sock = self.sockets.get_mut::<tcp::Socket>(l.handle);
            match sock.state() {
                tcp::State::Listen => {}
                tcp::State::Closed => {
                    let _ = sock.listen(IpListenEndpoint {
                        addr: None,
                        port: l.port,
                    });
                }
                _ => {
                    // Listening again cannot fail where it just succeeded;
                    // if it did, the socket is Closed and the next poll
                    // retries.
                    let mut next = new_tcp_socket();
                    let _ = next.listen(IpListenEndpoint {
                        addr: None,
                        port: l.port,
                    });
                    let remote = sock
                        .remote_endpoint()
                        .map(from_smol)
                        .unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
                    l.accepted.push_back((ConnId(l.handle), remote));
                    l.handle = self.sockets.add(next);
                }
            }
        }
    }

    /// The next IP packet to send to a peer.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.device.tx.pop_front()
    }

    /// How long until the stack's timers need a poll, if it has any.
    pub fn poll_delay(&mut self, now: Instant) -> Option<Duration> {
        self.iface
            .poll_delay(smol_instant(now), &self.sockets)
            .map(|d| Duration::from_micros(d.total_micros()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moves every packet each stack has to send to the other.
    fn exchange(a: &mut NetCore, b: &mut NetCore, now: Instant) {
        for _ in 0..8 {
            a.poll(now);
            b.poll(now);
            let mut moved = false;
            while let Some(p) = a.poll_transmit() {
                b.receive(p);
                moved = true;
            }
            while let Some(p) = b.poll_transmit() {
                a.receive(p);
                moved = true;
            }
            if !moved {
                break;
            }
        }
    }

    #[test]
    fn two_cores_talk_tcp_without_io() {
        let mut now = Instant::from_millis(1000);
        let mut a = NetCore::new(1, now);
        let mut b = NetCore::new(2, now);
        a.set_addresses(&[("100.64.0.1".parse().unwrap(), 10)]);
        b.set_addresses(&[("100.64.0.2".parse().unwrap(), 10)]);

        let l = b.listen(2300).unwrap();
        let c = a
            .connect("100.64.0.2:2300".parse().unwrap(), 50000)
            .unwrap();
        exchange(&mut a, &mut b, now);
        let (s, peer) = b.accept(l).expect("accept");
        assert_eq!(peer, "100.64.0.1:50000".parse().unwrap());
        assert!(!a.status(c).connecting);

        assert_eq!(a.send(c, b"hello"), 5);
        exchange(&mut a, &mut b, now);
        let mut buf = [0u8; 64];
        assert_eq!(b.recv(s, &mut buf), 5);
        assert_eq!(&buf[..5], b"hello");

        // More than the socket buffers hold: it goes as the reader drains.
        let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
        let (mut sent, mut got) = (0, Vec::new());
        while got.len() < big.len() {
            sent += b.send(s, &big[sent..]);
            now += Duration::from_millis(10);
            exchange(&mut a, &mut b, now);
            while a.can_recv(c) {
                let n = a.recv(c, &mut buf);
                got.extend_from_slice(&buf[..n]);
            }
        }
        assert_eq!(got, big);

        a.close(c);
        exchange(&mut a, &mut b, now);
        assert!(!b.status(s).may_recv);
        b.close(s);
        exchange(&mut a, &mut b, now);
        assert!(b.status(s).closed);
        b.remove(s);
        b.unlisten(l);
    }
}
