//! Tail loss probe for our TCP connections.
//!
//! smoltcp waits at least one second before it sends a lost segment again
//! (`RTTE_MIN_RTO`, a constant). A request and answer protocol over Wi-Fi
//! then stalls for a second whenever an answer is lost, and nothing else
//! follows it that could produce duplicate acknowledgements. USB/IP is such
//! a protocol: the PC sends one URB, waits for its answer, and a debug
//! probe's tools give up after one second.
//!
//! So the last data segment of each connection is kept, and sent once more
//! when no acknowledgement covers it within the probe timeout (as in RFC 8985).
//! The copy is the same segment again, so the peer takes it as an ordinary
//! retransmission. smoltcp never learns about it and keeps its own timers.

use crate::time::Instant;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::time::Duration;

/// The shortest probe timeout, as Linux's shortest retransmission timeout.
const MIN_PTO: Duration = Duration::from_millis(200);
/// A probed segment that is never acknowledged is forgotten after this long
/// (the connection went away; smoltcp deals with it).
const FORGET: Duration = Duration::from_secs(60);

/// Addresses and ports of one direction of a connection: source then
/// destination, each as address bytes followed by the port.
type Flow = Vec<u8>;

struct Tail {
    /// The sequence number after the segment's last byte.
    end: u32,
    packet: Vec<u8>,
    sent: Instant,
    probed: bool,
}

#[derive(Default)]
pub(super) struct TailProbe {
    tails: BTreeMap<Flow, Tail>,
    /// Smoothed round trip time, from segments acknowledged without a probe.
    srtt: Option<Duration>,
}

/// The fields of a TCP segment that matter here.
struct Segment {
    flow: Flow,
    reverse: Flow,
    seq: u32,
    ack: Option<u32>,
    payload: usize,
    rst: bool,
}

fn parse(p: &[u8]) -> Option<Segment> {
    let (src, dst, tcp) = match p.first()? >> 4 {
        4 => {
            let ihl = (p[0] as usize & 0x0f) * 4;
            let total = u16::from_be_bytes([*p.get(2)?, *p.get(3)?]) as usize;
            if *p.get(9)? != 6 || ihl < 20 || total > p.len() || total < ihl {
                return None;
            }
            (&p[12..16], &p[16..20], &p[ihl..total])
        }
        6 => {
            let len = u16::from_be_bytes([*p.get(4)?, *p.get(5)?]) as usize;
            if *p.get(6)? != 6 || 40 + len > p.len() {
                return None;
            }
            (&p[8..24], &p[24..40], &p[40..40 + len])
        }
        _ => return None,
    };
    if tcp.len() < 20 {
        return None;
    }
    let off = (tcp[12] >> 4) as usize * 4;
    if off < 20 || off > tcp.len() {
        return None;
    }
    let flags = tcp[13];
    let endpoint = |ip: &[u8], port: &[u8]| [ip, port].concat();
    let s = endpoint(src, &tcp[0..2]);
    let d = endpoint(dst, &tcp[2..4]);
    Some(Segment {
        flow: [s.as_slice(), d.as_slice()].concat(),
        reverse: [d.as_slice(), s.as_slice()].concat(),
        seq: u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]),
        ack: (flags & 0x10 != 0).then(|| u32::from_be_bytes([tcp[8], tcp[9], tcp[10], tcp[11]])),
        payload: tcp.len() - off,
        rst: flags & 0x04 != 0,
    })
}

/// `a` is at or after `b` in sequence space.
fn seq_ge(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) < 0x8000_0000
}

impl TailProbe {
    fn pto(&self) -> Duration {
        self.srtt.map_or(MIN_PTO, |s| (2 * s).max(MIN_PTO))
    }

    /// A packet we are sending.
    pub(super) fn on_transmit(&mut self, packet: &[u8], now: Instant) {
        let Some(s) = parse(packet) else { return };
        if s.rst {
            self.tails.remove(&s.flow);
            return;
        }
        if s.payload == 0 {
            return;
        }
        let end = s.seq.wrapping_add(s.payload as u32);
        // A retransmission of something older leaves the tail as it is.
        if let Some(t) = self.tails.get(&s.flow) {
            if seq_ge(t.end, end) {
                return;
            }
        }
        self.tails.insert(
            s.flow,
            Tail {
                end,
                packet: packet.to_vec(),
                sent: now,
                probed: false,
            },
        );
    }

    /// A packet from a peer, before the stack takes it.
    pub(super) fn on_receive(&mut self, packet: &[u8], now: Instant) {
        let Some(s) = parse(packet) else { return };
        if s.rst {
            self.tails.remove(&s.reverse);
            return;
        }
        let Some(ack) = s.ack else { return };
        let Some(t) = self.tails.get(&s.reverse) else {
            return;
        };
        if !seq_ge(ack, t.end) {
            return;
        }
        if !t.probed {
            let sample = now.duration_since(t.sent);
            self.srtt = Some(match self.srtt {
                Some(s) => (7 * s + sample) / 8,
                None => sample,
            });
        }
        self.tails.remove(&s.reverse);
    }

    /// The segments to send again now.
    pub(super) fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let pto = self.pto();
        let mut out = Vec::new();
        self.tails.retain(|_, t| {
            let waited = now.duration_since(t.sent);
            if !t.probed && waited >= pto {
                t.probed = true;
                out.push(t.packet.clone());
            }
            !(t.probed && waited >= FORGET)
        });
        out
    }

    /// How long until the next probe is due.
    pub(super) fn delay(&self, now: Instant) -> Option<Duration> {
        let pto = self.pto();
        self.tails
            .values()
            .filter(|t| !t.probed)
            .map(|t| (t.sent + pto).duration_since(now))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// An IPv4 TCP segment with ACK set.
    fn seg(
        src: [u8; 4],
        sport: u16,
        dst: [u8; 4],
        dport: u16,
        seq: u32,
        ack: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let total = 40 + data.len();
        let mut p = vec![
            0x45,
            0,
            (total >> 8) as u8,
            total as u8,
            0,
            0,
            0,
            0,
            64,
            6,
            0,
            0,
        ];
        p.extend_from_slice(&src);
        p.extend_from_slice(&dst);
        p.extend_from_slice(&sport.to_be_bytes());
        p.extend_from_slice(&dport.to_be_bytes());
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ack.to_be_bytes());
        p.extend_from_slice(&[0x50, 0x18, 0x20, 0x00, 0, 0, 0, 0]);
        p.extend_from_slice(data);
        p
    }

    const US: [u8; 4] = [100, 64, 0, 1];
    const PEER: [u8; 4] = [100, 64, 0, 2];

    #[test]
    fn an_unacknowledged_tail_is_sent_once_more() {
        let mut t = TailProbe::default();
        let t0 = Instant::from_millis(1000);
        let out = seg(US, 3240, PEER, 40000, 1000, 1, b"answer");
        t.on_transmit(&out, t0);
        assert_eq!(t.delay(t0), Some(MIN_PTO));
        assert!(t.poll(t0 + Duration::from_millis(199)).is_empty());
        assert_eq!(t.poll(t0 + MIN_PTO), vec![out.clone()]);
        // Only once; smoltcp's own timer takes it from there.
        assert!(t.poll(t0 + Duration::from_millis(900)).is_empty());
        assert_eq!(t.delay(t0 + Duration::from_millis(900)), None);
        // Forgotten when it is never acknowledged.
        t.poll(t0 + FORGET + MIN_PTO);
        assert!(t.tails.is_empty());
    }

    #[test]
    fn an_acknowledged_tail_is_not_probed_and_sets_the_rtt() {
        let mut t = TailProbe::default();
        let t0 = Instant::from_millis(1000);
        t.on_transmit(&seg(US, 3240, PEER, 40000, 1000, 1, b"answer"), t0);
        // A partial acknowledgement keeps it.
        t.on_receive(
            &seg(PEER, 40000, US, 3240, 1, 1003, b""),
            t0 + Duration::from_millis(5),
        );
        assert_eq!(t.tails.len(), 1);
        // Another connection's acknowledgement does not count.
        t.on_receive(
            &seg(PEER, 40001, US, 3240, 1, 1006, b""),
            t0 + Duration::from_millis(6),
        );
        assert_eq!(t.tails.len(), 1);
        t.on_receive(
            &seg(PEER, 40000, US, 3240, 1, 1006, b"next"),
            t0 + Duration::from_millis(150),
        );
        assert!(t.tails.is_empty());
        assert!(t.poll(t0 + Duration::from_secs(2)).is_empty());
        // A 150 ms round trip makes the probe wait 300 ms.
        assert_eq!(t.pto(), Duration::from_millis(300));
    }

    #[test]
    fn only_newer_data_replaces_the_tail() {
        let mut t = TailProbe::default();
        let t0 = Instant::from_millis(1000);
        let first = seg(US, 3240, PEER, 40000, 0xffff_fff0, 1, &[0; 32]);
        t.on_transmit(&first, t0);
        // Pure acknowledgements and retransmissions of older data leave it.
        t.on_transmit(&seg(US, 3240, PEER, 40000, 0x10, 2, b""), t0);
        t.on_transmit(&seg(US, 3240, PEER, 40000, 0xffff_fff0, 2, &[0; 8]), t0);
        assert_eq!(t.tails.values().next().unwrap().end, 0x10);
        // Newer data across the sequence wrap replaces it.
        let second = seg(US, 3240, PEER, 40000, 0x10, 2, b"more");
        t.on_transmit(&second, t0 + Duration::from_millis(50));
        assert_eq!(t.poll(t0 + Duration::from_millis(250)), vec![second]);
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut t = TailProbe::default();
        let p = seg(US, 3240, PEER, 40000, 1, 1, b"x");
        for cut in 0..p.len() {
            t.on_transmit(&p[..cut], Instant::from_millis(0));
            t.on_receive(&p[..cut], Instant::from_millis(0));
        }
        t.on_transmit(&[0x60; 3], Instant::from_millis(0));
        t.on_transmit(&[], Instant::from_millis(0));
    }
}
