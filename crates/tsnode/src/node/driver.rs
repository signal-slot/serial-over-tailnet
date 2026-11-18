//! Runs [`NodeCore`] on `std`: a control thread that opens connections to
//! the control server, sends what the core asks for and hands it the
//! answers. The core's lock is never held across I/O, so `status()` answers
//! at once.

use super::*;
use crate::control::ControlClient;
use crate::magicsock::MagicSock;
use crate::net::Dialer;
use crate::netstack::{NetStack, TcpListener};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Persistent key/value storage for node identity.
pub trait Store: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str) -> io::Result<()>;
    fn remove(&self, key: &str) -> io::Result<()>;
}

/// How long one wait for a map message lasts: the core's timers run
/// between waits.
const MAP_POLL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
pub struct NodeConfig {
    pub control_url: String,
    /// Auth key used for headless registration; may be absent once registered.
    pub auth_key: Option<String>,
    pub hostname: String,
    pub version: String,
    /// UDP port for WireGuard/disco (0 = ephemeral).
    pub udp_port: u16,
    /// LAN IP to advertise; auto-detected when `None`.
    pub local_ip: Option<IpAddr>,
    /// Stack size for worker threads (UDP, DERP, tick, netstack).
    pub worker_stack: usize,
    /// Stack size for the control thread (TLS + JSON).
    pub control_stack: usize,
    /// DERP region code used when STUN gets no answers (e.g. "tok").
    pub fallback_region_code: String,
    /// Thread spawner (defaults to `std::thread`).
    pub spawner: Option<crate::net::Spawner>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            control_url: "https://controlplane.tailscale.com".into(),
            auth_key: None,
            hostname: "tsnode".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            udp_port: 41641,
            local_ip: None,
            worker_stack: 32 * 1024,
            control_stack: 64 * 1024,
            fallback_region_code: "tok".into(),
            spawner: None,
        }
    }
}

struct Inner {
    control_url: String,
    store: Arc<dyn Store>,
    dialer: Arc<dyn Dialer>,
    magicsock: MagicSock,
    netstack: NetStack,
    machine_key: KeyPair,
    core: Mutex<NodeCore>,
    shutdown: AtomicBool,
    /// Set by magicsock's callback; handed to the core by the control
    /// thread (the callback can run while the core is locked).
    endpoints_dirty: AtomicBool,
}

#[derive(Clone)]
pub struct Node {
    inner: Arc<Inner>,
}

fn load_or_create_key(store: &dyn Store, name: &str) -> io::Result<KeyPair> {
    if let Some(v) = store.get(name) {
        if let Some(k) = KeyPair::from_secret_hex(&v) {
            return Ok(k);
        }
        warn!("node: stored {name} is invalid; regenerating");
    }
    let k = KeyPair::generate();
    store.set(name, &k.secret_hex())?;
    Ok(k)
}

impl Node {
    pub fn start(
        cfg: NodeConfig,
        store: Arc<dyn Store>,
        dialer: Arc<dyn Dialer>,
    ) -> io::Result<Node> {
        let machine_key = load_or_create_key(store.as_ref(), KEY_MACHINE)?;
        let node_key = load_or_create_key(store.as_ref(), KEY_NODE)?;
        let disco_key = load_or_create_key(store.as_ref(), KEY_DISCO)?;
        info!(
            "node: machine {} node {}",
            crate::keys::short(machine_key.public()),
            crate::keys::short(node_key.public())
        );

        let spawner = cfg.spawner.clone().unwrap_or_else(crate::net::std_spawner);
        let magicsock = MagicSock::start(
            node_key.clone(),
            disco_key,
            dialer.clone(),
            cfg.udp_port,
            cfg.local_ip,
            cfg.worker_stack,
            spawner.clone(),
        )?;
        let ms = magicsock.clone();
        let netstack = NetStack::start(
            Box::new(move |pkt| ms.send_ip(&pkt)),
            cfg.worker_stack,
            &spawner,
        )?;
        let ns = netstack.clone();
        magicsock.set_ip_sink(Arc::new(move |pkt| ns.inject(pkt)));

        let core = NodeCore::new(
            NodeSettings {
                hostname: cfg.hostname.clone(),
                version: cfg.version.clone(),
                auth_key: cfg.auth_key.clone(),
                fallback_region_code: cfg.fallback_region_code.clone(),
            },
            node_key,
            store.get(KEY_REGISTERED).as_deref() == Some("1"),
            store.get(KEY_HOME_DERP).and_then(|v| v.parse().ok()),
        );
        let inner = Arc::new(Inner {
            control_url: cfg.control_url.clone(),
            store,
            dialer,
            magicsock,
            netstack,
            machine_key,
            core: Mutex::new(core),
            shutdown: AtomicBool::new(false),
            endpoints_dirty: AtomicBool::new(false),
        });
        let node = Node {
            inner: inner.clone(),
        };
        let flag = Arc::downgrade(&inner);
        inner.magicsock.set_endpoints_changed(Arc::new(move || {
            if let Some(i) = flag.upgrade() {
                i.endpoints_dirty.store(true, Ordering::Relaxed);
            }
        }));
        let ctl = node.clone();
        spawner(
            "control",
            cfg.control_stack,
            Box::new(move || ctl.control_loop()),
        )?;
        Ok(node)
    }

    pub fn status(&self) -> NodeStatus {
        let core = self.core();
        let mut s = self
            .inner
            .magicsock
            .inspect(|m| core.status(m, Instant::now()));
        drop(core);
        s.derp_connected = s.home_derp != 0 && self.inner.magicsock.derp_connected(s.home_derp);
        s
    }

    pub fn listen(&self, port: u16) -> TcpListener {
        self.inner.netstack.listen(port)
    }

    pub fn netstack(&self) -> &NetStack {
        &self.inner.netstack
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        self.inner.magicsock.shutdown();
        self.inner.netstack.shutdown();
    }

    fn core(&self) -> MutexGuard<'_, NodeCore> {
        self.inner.core.lock().unwrap()
    }

    /// Runs `f` on the node's core and magicsock's, then carries out the
    /// actions the core asked for.
    fn with_cores<R>(&self, f: impl FnOnce(&mut NodeCore, &mut MagicCore, Instant) -> R) -> R {
        let mut core = self.core();
        if self.inner.endpoints_dirty.swap(false, Ordering::Relaxed) {
            core.endpoints_changed();
        }
        let r = self
            .inner
            .magicsock
            .with_core(|m| f(&mut core, m, Instant::now()));
        let actions: Vec<Action> = core::iter::from_fn(|| core.poll_action()).collect();
        drop(core);
        for a in actions {
            match a {
                Action::Store { key, value } => {
                    let r = match value {
                        Some(v) => self.inner.store.set(key, &v),
                        None => self.inner.store.remove(key),
                    };
                    if let Err(e) = r {
                        warn!("node: cannot store {key}: {e}");
                    }
                }
                Action::Addresses(cidrs) => self.inner.netstack.set_addresses(cidrs),
            }
        }
        r
    }

    fn sleep_backoff(&self, backoff: &mut std::time::Duration) {
        let d = *backoff;
        let until = std::time::Instant::now() + d;
        while std::time::Instant::now() < until && !self.inner.shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        *backoff = (d * 2).min(std::time::Duration::from_secs(60));
    }

    fn control_loop(&self) {
        let mut backoff = std::time::Duration::from_secs(2);
        let inner = &self.inner;
        while !inner.shutdown.load(Ordering::Relaxed) {
            self.core().connecting();
            let client = match ControlClient::connect(
                inner.dialer.clone(),
                &inner.control_url,
                inner.machine_key.clone(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    self.core().failed(format!("control: {e}"));
                    self.sleep_backoff(&mut backoff);
                    continue;
                }
            };

            if self.core().needs_registration() {
                match self.register(&client) {
                    Ok(Registration::Authorized) => {}
                    Ok(_) => {
                        // Waiting for interactive approval; poll again later.
                        std::thread::sleep(std::time::Duration::from_secs(20));
                        continue;
                    }
                    Err(e) => {
                        self.core().failed(format!("register: {e}"));
                        self.sleep_backoff(&mut backoff);
                        continue;
                    }
                }
            }

            match self.run_map_stream(&client) {
                Ok(()) => backoff = std::time::Duration::from_secs(2),
                Err(e) => {
                    if self.with_cores(|n, _, _| n.map_ended(&e.to_string())) {
                        backoff = std::time::Duration::from_secs(2);
                    }
                    self.sleep_backoff(&mut backoff);
                }
            }
        }
        debug!("node: control loop exit");
    }

    /// Registers, following a key rotation if control asks for one.
    fn register(&self, client: &ControlClient) -> io::Result<Registration> {
        loop {
            let req = self.core().register_request();
            let resp = client.register(&req)?;
            let r = self.with_cores(|n, m, _| n.on_register_response(resp, m));
            match r {
                Ok(Registration::Again) => continue,
                Ok(r) => return Ok(r),
                Err(e) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, e)),
            }
        }
    }

    fn run_map_stream(&self, client: &ControlClient) -> io::Result<()> {
        let inner = &self.inner;
        let req = self.with_cores(|n, m, now| {
            n.starting_map(m, now);
            n.map_request(true, m)
        });
        let mut stream = client.map_stream(&req)?;
        self.with_cores(|n, _, now| n.map_connected(now));
        loop {
            if inner.shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            match stream.next(MAP_POLL) {
                Ok(Some(resp)) => self.with_cores(|n, m, now| n.on_map_response(resp, m, now)),
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "map stream closed",
                    ))
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
            let (silent, lite) = self.with_cores(|n, m, now| {
                n.tick(m, now);
                (n.map_silent(now), n.lite_update(m, now))
            });
            if silent {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no keep-alive from control",
                ));
            }
            if let Some(lite) = lite {
                if let Err(e) = client.map_once(&lite) {
                    warn!("node: endpoint update failed: {e}");
                    self.core().lite_update_failed();
                }
            }
        }
    }
}
