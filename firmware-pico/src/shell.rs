//! The setup shell: adapter-core's [`Shell`], the same as on the ESP32
//! boards, run on core 1.
//!
//! The shell is written against blocking calls, so it gets a core of its
//! own and waits by spinning. What needs the radio, the network stack or a
//! flash write is handed to the Wi-Fi task on core 0 ([`crate::wifi`]) as a
//! [`Request`], and the shell waits for the [`Response`].

use crate::{config, console};
use adapter_core::shell::{
    ApInfo, Console, NodeState, Platform, SavedConfig, Shell, StatusSnapshot, WifiNetwork,
};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::time::Duration;
use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

/// Work for core 0.
pub enum Request {
    Scan,
    Connect { ssid: String, password: String },
    Reconnect,
    Save(SavedConfig),
    FactoryReset,
    StartNode,
}

pub enum Response {
    Scan(Result<Vec<ApInfo>, String>),
    /// The address after joining a network.
    Address(Result<String, String>),
    Done(Result<(), String>),
}

pub static REQUESTS: Channel<CriticalSectionRawMutex, Request, 1> = Channel::new();
pub static RESPONSES: Channel<CriticalSectionRawMutex, Response, 1> = Channel::new();

/// What `status` shows on the Wi-Fi line; the Wi-Fi task keeps it current.
static WIFI_STATE: Mutex<RefCell<String>> = Mutex::new(RefCell::new(String::new()));
/// Why the chip last reset, for `status`.
static LAST_RESET: Mutex<RefCell<String>> = Mutex::new(RefCell::new(String::new()));

pub fn set_wifi_state(s: String) {
    critical_section::with(|cs| *WIFI_STATE.borrow_ref_mut(cs) = s);
}

pub fn set_last_reset(s: String) {
    critical_section::with(|cs| *LAST_RESET.borrow_ref_mut(cs) = s);
}

fn spin(d: Duration) {
    embassy_time::block_for(embassy_time::Duration::from_micros(d.as_micros() as u64));
}

/// Hands `req` to core 0 and waits for the answer.
fn call(req: Request) -> Response {
    let mut req = Some(req);
    while let Some(r) = req.take() {
        if let Err(embassy_sync::channel::TrySendError::Full(r)) = REQUESTS.try_send(r) {
            req = Some(r);
            spin(Duration::from_millis(2));
        }
    }
    loop {
        if let Ok(r) = RESPONSES.try_receive() {
            return r;
        }
        spin(Duration::from_millis(2));
    }
}

/// Keys of the shell's settings in [`config`]; the node's identity lives
/// beside them under other keys.
const K_WIFI: &str = "wifi.";
const K_HOST: &str = "hostname";
const K_AUTHKEY: &str = "authkey";
const K_PORT: &str = "port";
const K_BAUD: &str = "baud";
const K_USBDRV: &str = "usbdrv";

/// The shell's settings as stored; `None` until a Wi-Fi network is known,
/// as on the ESP32.
pub fn saved_config() -> Option<SavedConfig> {
    let v = config::values();
    let networks: Vec<WifiNetwork> = v
        .iter()
        .filter(|(k, _)| k.starts_with(K_WIFI))
        .filter_map(|(_, n)| {
            let (ssid, password) = n.split_once('\t')?;
            Some(WifiNetwork {
                ssid: ssid.to_string(),
                password: password.to_string(),
            })
        })
        .collect();
    if networks.is_empty() {
        return None;
    }
    Some(SavedConfig {
        networks,
        hostname: v.get(K_HOST).cloned().unwrap_or_default(),
        auth_key: v.get(K_AUTHKEY).cloned(),
        tcp_port: v.get(K_PORT).and_then(|p| p.parse().ok()),
        baud_rate: v.get(K_BAUD).and_then(|b| b.parse().ok()),
        usb_driver: v.get(K_USBDRV).cloned(),
    })
}

/// Writes `c` into the stored values, replacing the shell's settings.
pub fn store_config(v: &mut BTreeMap<String, String>, c: &SavedConfig) {
    v.retain(|k, _| {
        !(k.starts_with(K_WIFI)
            || [K_HOST, K_AUTHKEY, K_PORT, K_BAUD, K_USBDRV].contains(&k.as_str()))
    });
    for (i, n) in c.networks.iter().enumerate() {
        v.insert(
            format!("{K_WIFI}{i}"),
            format!("{}\t{}", n.ssid, n.password),
        );
    }
    if !c.hostname.is_empty() {
        v.insert(K_HOST.into(), c.hostname.clone());
    }
    let optional = [
        (K_AUTHKEY, c.auth_key.clone()),
        (K_PORT, c.tcp_port.map(|p| p.to_string())),
        (K_BAUD, c.baud_rate.map(|b| b.to_string())),
        (K_USBDRV, c.usb_driver.clone()),
    ];
    for (k, val) in optional {
        if let Some(val) = val {
            v.insert(k.into(), val);
        }
    }
}

struct UsbConsole;

impl Console for UsbConsole {
    fn read_byte(&mut self, timeout: Duration) -> Option<u8> {
        let mut waited = Duration::ZERO;
        loop {
            if let Some(b) = console::try_read_byte() {
                return Some(b);
            }
            if waited >= timeout {
                return None;
            }
            spin(Duration::from_millis(5));
            waited += Duration::from_millis(5);
        }
    }

    fn write(&mut self, s: &str) {
        console::print(s);
    }
}

struct PicoPlatform;

impl Platform for PicoPlatform {
    fn status(&self) -> StatusSnapshot {
        let node = crate::node::snapshot();
        StatusSnapshot {
            firmware: env!("CARGO_PKG_VERSION").into(),
            setup_mode: false,
            wifi: critical_section::with(|cs| WIFI_STATE.borrow_ref(cs).clone()),
            node: Some(self.node_state()),
            derp: node.as_ref().map_or("-".into(), |s| s.derp.clone()),
            peers: node.as_ref().map_or("-".into(), |s| s.peers.clone()),
            usb: "console (this board has no USB host)".into(),
            heap: format!(
                "free {} KB, used {} KB",
                crate::HEAP.free() / 1024,
                crate::HEAP.used() / 1024
            ),
            last_reset: critical_section::with(|cs| LAST_RESET.borrow_ref(cs).clone()),
            board: "Raspberry Pi Pico 2 W".into(),
        }
    }

    fn scan_wifi(&self) -> Result<Vec<ApInfo>, String> {
        match call(Request::Scan) {
            Response::Scan(r) => r,
            _ => Err("internal error".into()),
        }
    }

    fn connect_wifi(&self, ssid: &str, password: &str) -> Result<String, String> {
        match call(Request::Connect {
            ssid: ssid.into(),
            password: password.into(),
        }) {
            Response::Address(r) => r,
            _ => Err("internal error".into()),
        }
    }

    fn reconnect_wifi(&self) -> Result<String, String> {
        match call(Request::Reconnect) {
            Response::Address(r) => r,
            _ => Err("internal error".into()),
        }
    }

    fn config(&self) -> Option<SavedConfig> {
        saved_config()
    }

    fn save_config(&self, cfg: &SavedConfig) -> Result<(), String> {
        match call(Request::Save(cfg.clone())) {
            Response::Done(r) => r,
            _ => Err("internal error".into()),
        }
    }

    fn start_node(&self) -> Result<(), String> {
        match call(Request::StartNode) {
            Response::Done(r) => r,
            _ => Err("internal error".into()),
        }
    }

    fn node_state(&self) -> NodeState {
        crate::node::snapshot().map_or(NodeState::Off, |s| s.state)
    }

    fn is_registered(&self) -> bool {
        config::values()
            .get(tsnode::node::KEY_REGISTERED)
            .map(String::as_str)
            == Some("1")
    }

    fn factory_reset(&self) -> Result<(), String> {
        match call(Request::FactoryReset) {
            Response::Done(r) => r,
            _ => Err("internal error".into()),
        }
    }

    fn reboot(&self) -> ! {
        // Let the console send what the shell just wrote.
        spin(Duration::from_millis(200));
        crate::panic::reset_chip()
    }
}

/// Core 1's program: the shell, forever.
pub fn run() -> ! {
    let mut console = UsbConsole;
    let platform = PicoPlatform;
    let first_setup = saved_config().is_none();
    let mut shell = Shell::new(&mut console, &platform);
    shell.run(first_setup, false);
    unreachable!()
}
