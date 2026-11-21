//! Runs [`NetCore`] on a thread for `std`, with blocking [`TcpListener`]
//! and [`TcpStream`] handles. Each connection keeps its own byte queues
//! under its own lock, so readers and writers never wait for the stack.

use super::*;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const USER_TX_LIMIT: usize = if cfg!(feature = "small-ram") {
    8 * 1024
} else {
    32 * 1024
};
/// Bytes buffered for a reader that is not keeping up; beyond this the
/// stack stops draining the socket and TCP flow control holds the sender.
const USER_RX_LIMIT: usize = if cfg!(feature = "small-ram") {
    4 * 1024
} else {
    16 * 1024
};
/// Longest sleep of the poll thread when nothing wakes it.
const MAX_POLL_DELAY: Duration = Duration::from_millis(500);

struct ConnState {
    rx: VecDeque<u8>,
    tx: VecDeque<u8>,
    peer_closed: bool,
    local_closed: bool,
    aborted: bool,
    dead: bool,
    remote: SocketAddr,
}

impl ConnState {
    fn new(remote: SocketAddr) -> Self {
        ConnState {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
            peer_closed: false,
            local_closed: false,
            aborted: false,
            dead: false,
            remote,
        }
    }
}

struct ConnShared {
    m: Mutex<ConnState>,
    cv: Condvar,
}

struct ListenerShared {
    port: u16,
    queue: Mutex<VecDeque<TcpStream>>,
    cv: Condvar,
    closed: Mutex<bool>,
}

struct State {
    inbound: VecDeque<Vec<u8>>,
    /// Addresses to set at the next poll.
    addrs: Option<Vec<(IpAddr, u8)>>,
    new_listeners: Vec<Arc<ListenerShared>>,
    new_connects: Vec<(SocketAddr, Arc<ConnShared>)>,
    shutdown: bool,
    wake: bool,
}

struct Inner {
    state: Mutex<State>,
    cv: Condvar,
}

/// Handle to the stack; cheap to clone.
#[derive(Clone)]
pub struct NetStack {
    inner: Arc<Inner>,
}

impl NetStack {
    /// Starts the poll thread. `outbound` is called with every IP packet the
    /// stack wants to send (from the poll thread, without locks held).
    pub fn start(
        outbound: Box<dyn Fn(Vec<u8>) + Send + Sync>,
        stack_size: usize,
        spawner: &crate::net::Spawner,
    ) -> io::Result<NetStack> {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                inbound: VecDeque::new(),
                addrs: None,
                new_listeners: Vec::new(),
                new_connects: Vec::new(),
                shutdown: false,
                wake: false,
            }),
            cv: Condvar::new(),
        });
        let ns = NetStack {
            inner: inner.clone(),
        };
        spawner(
            "netstack",
            stack_size,
            Box::new(move || poll_loop(inner, outbound)),
        )?;
        Ok(ns)
    }

    /// Sets the node's tailnet addresses (with their on-link prefixes).
    pub fn set_addresses(&self, addrs: Vec<(IpAddr, u8)>) {
        let mut st = self.inner.state.lock().unwrap();
        st.addrs = Some(addrs);
        st.wake = true;
        self.inner.cv.notify_all();
    }

    /// Delivers an IP packet received from a peer.
    pub fn inject(&self, pkt: Vec<u8>) {
        let mut st = self.inner.state.lock().unwrap();
        if st.inbound.len() < 256 {
            st.inbound.push_back(pkt);
        }
        st.wake = true;
        self.inner.cv.notify_all();
    }

    pub fn listen(&self, port: u16) -> TcpListener {
        let shared = Arc::new(ListenerShared {
            port,
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            closed: Mutex::new(false),
        });
        let mut st = self.inner.state.lock().unwrap();
        st.new_listeners.push(shared.clone());
        st.wake = true;
        self.inner.cv.notify_all();
        TcpListener {
            shared,
            stack: self.clone(),
        }
    }

    /// Active open to `remote`. Data written before the handshake completes is queued.
    pub fn connect(&self, remote: SocketAddr) -> TcpStream {
        let shared = Arc::new(ConnShared {
            m: Mutex::new(ConnState::new(remote)),
            cv: Condvar::new(),
        });
        let mut st = self.inner.state.lock().unwrap();
        st.new_connects.push((remote, shared.clone()));
        st.wake = true;
        self.inner.cv.notify_all();
        TcpStream {
            shared,
            stack: self.clone(),
        }
    }

    pub fn shutdown(&self) {
        let mut st = self.inner.state.lock().unwrap();
        st.shutdown = true;
        st.wake = true;
        self.inner.cv.notify_all();
    }

    fn wake(&self) {
        let mut st = self.inner.state.lock().unwrap();
        st.wake = true;
        self.inner.cv.notify_all();
    }
}

pub struct TcpListener {
    shared: Arc<ListenerShared>,
    stack: NetStack,
}

impl TcpListener {
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// Waits up to `timeout` for a new connection.
    pub fn accept(&self, timeout: Duration) -> Option<TcpStream> {
        let mut q = self.shared.queue.lock().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(c) = q.pop_front() {
                return Some(c);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            q = self.shared.cv.wait_timeout(q, deadline - now).unwrap().0;
        }
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        *self.shared.closed.lock().unwrap() = true;
        self.stack.wake();
    }
}

pub struct TcpStream {
    shared: Arc<ConnShared>,
    stack: NetStack,
}

impl TcpStream {
    pub fn peer_addr(&self) -> SocketAddr {
        self.shared.m.lock().unwrap().remote
    }

    /// Reads available bytes. Returns `Ok(0)` once the peer has closed and the
    /// buffer is drained; `TimedOut` if nothing arrived within `timeout`.
    pub fn read(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let mut st = self.shared.m.lock().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if !st.rx.is_empty() {
                let n = buf.len().min(st.rx.len());
                for (i, b) in st.rx.drain(..n).enumerate() {
                    buf[i] = b;
                }
                let was_full = st.rx.len() + n >= USER_RX_LIMIT;
                drop(st);
                if was_full {
                    // Room again: let the poll loop drain the socket.
                    self.stack.wake();
                }
                return Ok(n);
            }
            if st.peer_closed || st.dead {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "read timeout"));
            }
            st = self.shared.cv.wait_timeout(st, deadline - now).unwrap().0;
        }
    }

    /// Queues `data` for sending; blocks while the send queue is full.
    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut st = self.shared.m.lock().unwrap();
        let mut offset = 0;
        while offset < data.len() {
            if st.dead || st.local_closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "connection closed",
                ));
            }
            let room = USER_TX_LIMIT.saturating_sub(st.tx.len());
            if room == 0 {
                st = self
                    .shared
                    .cv
                    .wait_timeout(st, Duration::from_millis(200))
                    .unwrap()
                    .0;
                continue;
            }
            let n = room.min(data.len() - offset);
            st.tx.extend(&data[offset..offset + n]);
            offset += n;
            drop(st);
            self.stack.wake();
            st = self.shared.m.lock().unwrap();
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        let st = self.shared.m.lock().unwrap();
        st.dead || st.peer_closed
    }

    /// Graceful close after pending data is sent.
    pub fn close(&self) {
        self.shared.m.lock().unwrap().local_closed = true;
        self.stack.wake();
    }

    pub fn abort(&self) {
        let mut st = self.shared.m.lock().unwrap();
        st.aborted = true;
        st.local_closed = true;
        drop(st);
        self.stack.wake();
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        self.close();
    }
}

fn poll_loop(inner: Arc<Inner>, outbound: Box<dyn Fn(Vec<u8>) + Send + Sync>) {
    let seed = u64::from_le_bytes(crate::crypto::random_array());
    let mut core = NetCore::new(seed, Instant::now());
    let mut listeners: Vec<(Arc<ListenerShared>, ListenerId)> = Vec::new();
    let mut conns: Vec<(ConnId, Arc<ConnShared>)> = Vec::new();

    loop {
        // Pull work from the shared state.
        let connects = {
            let mut st = inner.state.lock().unwrap();
            if st.shutdown {
                break;
            }
            st.wake = false;
            while let Some(p) = st.inbound.pop_front() {
                core.receive(p);
            }
            if let Some(addrs) = st.addrs.take() {
                core.set_addresses(&addrs);
            }
            for l in st.new_listeners.drain(..) {
                if let Some(id) = core.listen(l.port) {
                    listeners.push((l, id));
                }
            }
            std::mem::take(&mut st.new_connects)
        };
        for (remote, shared) in connects {
            let local_port = 49152 + (u16::from_le_bytes(crate::crypto::random_array()) % 16000);
            match core.connect(remote, local_port) {
                Some(id) => conns.push((id, shared)),
                None => {
                    shared.m.lock().unwrap().dead = true;
                    shared.cv.notify_all();
                }
            }
        }

        let now = Instant::now();
        core.poll(now);

        // Listeners: closed ones go, accepted connections get a stream.
        listeners.retain(|(shared, id)| {
            if *shared.closed.lock().unwrap() {
                core.unlisten(*id);
                return false;
            }
            while let Some((conn, remote)) = core.accept(*id) {
                let cs = Arc::new(ConnShared {
                    m: Mutex::new(ConnState::new(remote)),
                    cv: Condvar::new(),
                });
                conns.push((conn, cs.clone()));
                let stream = TcpStream {
                    shared: cs,
                    stack: NetStack {
                        inner: inner.clone(),
                    },
                };
                shared.queue.lock().unwrap().push_back(stream);
                shared.cv.notify_all();
            }
            true
        });

        // Connections: move bytes between the sockets and the user queues.
        conns.retain(|(id, shared)| {
            let id = *id;
            let mut st = shared.m.lock().unwrap();
            let mut changed = false;
            let mut buf = [0u8; 1024];
            while core.can_recv(id) && st.rx.len() < USER_RX_LIMIT {
                let n = core.recv(id, &mut buf);
                if n == 0 {
                    break;
                }
                st.rx.extend(&buf[..n]);
                changed = true;
            }
            while core.can_send(id) && !st.tx.is_empty() {
                let (a, _) = st.tx.as_slices();
                let n = core.send(id, &a[..a.len().min(1024)]);
                if n == 0 {
                    break;
                }
                st.tx.drain(..n);
                changed = true;
            }
            if st.aborted {
                core.abort(id);
            } else if st.local_closed && st.tx.is_empty() {
                core.close(id);
            }
            let status = core.status(id);
            if !status.may_recv && !st.peer_closed && !status.connecting {
                st.peer_closed = true;
                changed = true;
            }
            if status.closed {
                st.dead = true;
                st.peer_closed = true;
                changed = true;
            }
            if changed {
                shared.cv.notify_all();
            }
            drop(st);
            if status.closed {
                core.remove(id);
            }
            !status.closed
        });

        while let Some(pkt) = core.poll_transmit() {
            outbound(pkt);
        }

        let delay = core
            .poll_delay(now)
            .unwrap_or(MAX_POLL_DELAY)
            .min(MAX_POLL_DELAY);
        let st = inner.state.lock().unwrap();
        if !st.wake && !st.shutdown {
            let _ = inner.cv.wait_timeout(st, delay).unwrap();
        }
    }
    debug!("netstack: stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two stacks wired back to back: whatever A sends, B receives and vice versa.
    #[test]
    fn tcp_over_two_stacks() {
        let a_ref: Arc<Mutex<Option<NetStack>>> = Arc::new(Mutex::new(None));
        let b_ref: Arc<Mutex<Option<NetStack>>> = Arc::new(Mutex::new(None));
        let (b_for_a, a_for_b) = (b_ref.clone(), a_ref.clone());
        let a = NetStack::start(
            Box::new(move |p| {
                if let Some(b) = b_for_a.lock().unwrap().as_ref() {
                    b.inject(p)
                }
            }),
            256 * 1024,
            &crate::net::std_spawner(),
        )
        .unwrap();
        let b = NetStack::start(
            Box::new(move |p| {
                if let Some(a) = a_for_b.lock().unwrap().as_ref() {
                    a.inject(p)
                }
            }),
            256 * 1024,
            &crate::net::std_spawner(),
        )
        .unwrap();
        *a_ref.lock().unwrap() = Some(a.clone());
        *b_ref.lock().unwrap() = Some(b.clone());
        a.set_addresses(vec![("100.64.0.1".parse().unwrap(), 10)]);
        b.set_addresses(vec![("100.64.0.2".parse().unwrap(), 10)]);

        let listener = b.listen(2300);
        let client = a.connect("100.64.0.2:2300".parse().unwrap());
        let server = listener.accept(Duration::from_secs(5)).expect("accept");
        assert_eq!(
            server.peer_addr().ip(),
            "100.64.0.1".parse::<IpAddr>().unwrap()
        );

        client.write_all(b"hello over smoltcp").unwrap();
        let mut buf = [0u8; 64];
        let n = server.read(&mut buf, Duration::from_secs(5)).unwrap();
        assert_eq!(&buf[..n], b"hello over smoltcp");

        // Written from another thread: more than the buffers hold (far more
        // with `small-ram`), so the writer has to wait for the reader.
        let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
        let (to_send, server) = (big.clone(), Arc::new(server));
        let sender = server.clone();
        let writer = std::thread::spawn(move || sender.write_all(&to_send).unwrap());
        let mut got = Vec::new();
        while got.len() < big.len() {
            let n = client.read(&mut buf, Duration::from_secs(5)).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, big);
        writer.join().unwrap();

        client.close();
        let n = server.read(&mut buf, Duration::from_secs(5)).unwrap();
        assert_eq!(n, 0);
        a.shutdown();
        b.shutdown();
    }
}
