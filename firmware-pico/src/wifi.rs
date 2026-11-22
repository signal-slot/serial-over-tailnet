//! Core 0's side of the shell and the Wi-Fi link: owns the cyw43 control
//! handle and the network stack, joins the known networks at boot, and
//! carries out the shell's [`Request`]s.

use crate::config;
use crate::shell::{self, Request, Response};
use adapter_core::shell::{ApInfo, WifiNetwork};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use cyw43::{Control, JoinOptions, ScanOptions};
use embassy_net::Stack;
use embassy_time::{with_timeout, Duration};
use log::{info, warn};

const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Wifi {
    control: Control<'static>,
    stack: Stack<'static>,
    joined: bool,
}

impl Wifi {
    pub fn new(control: Control<'static>, stack: Stack<'static>) -> Self {
        Wifi {
            control,
            stack,
            joined: false,
        }
    }

    async fn scan(&mut self) -> Result<Vec<ApInfo>, String> {
        let mut found = Vec::new();
        let mut scanner = self.control.scan(ScanOptions::default()).await;
        let r = with_timeout(SCAN_TIMEOUT, async {
            while let Some(bss) = scanner.next().await {
                let len = (bss.ssid_len as usize).min(32);
                let ssid = String::from_utf8_lossy(&bss.ssid[..len]).into_owned();
                found.push(ApInfo {
                    ssid,
                    rssi: bss.rssi.clamp(-128, 127) as i8,
                    // The privacy bit of the capability field.
                    secure: bss.capability & 0x10 != 0,
                });
            }
        })
        .await;
        drop(scanner);
        if r.is_err() && found.is_empty() {
            return Err("scan timed out".into());
        }
        Ok(found)
    }

    async fn connect(&mut self, ssid: &str, password: &str) -> Result<String, String> {
        if self.joined {
            self.control.leave().await;
            self.joined = false;
        }
        shell::set_wifi_state(format!("connecting to {ssid}"));
        let opts = if password.is_empty() {
            JoinOptions::new_open()
        } else {
            JoinOptions::new(password.as_bytes())
        };
        let r = match with_timeout(JOIN_TIMEOUT, self.control.join(ssid, opts)).await {
            Ok(Ok(())) => {
                self.joined = true;
                match with_timeout(DHCP_TIMEOUT, self.stack.wait_config_up()).await {
                    Ok(()) => match self.stack.config_v4() {
                        Some(c) => Ok(c.address.address().to_string()),
                        None => Err("no IPv4 address".to_string()),
                    },
                    Err(_) => Err("no address from DHCP".to_string()),
                }
            }
            Ok(Err(e)) => Err(format!("join failed ({e:?})")),
            Err(_) => Err("join timed out".to_string()),
        };
        match &r {
            Ok(ip) => {
                info!("wifi: connected to {ssid} ({ip})");
                shell::set_wifi_state(format!("connected to {ssid} ({ip})"));
            }
            Err(e) => {
                warn!("wifi: {ssid}: {e}");
                shell::set_wifi_state(format!("not connected ({ssid}: {e})"));
            }
        }
        r
    }

    /// Joins the strongest visible known network (all of them in turn if the
    /// scan sees none).
    async fn reconnect(&mut self) -> Result<String, String> {
        let known: Vec<WifiNetwork> = shell::saved_config()
            .map(|c| c.networks)
            .unwrap_or_default();
        if known.is_empty() {
            return Err("no known networks".into());
        }
        let visible = self.scan().await.unwrap_or_default();
        let mut order: Vec<(i32, &WifiNetwork)> = known
            .iter()
            .map(|n| {
                let rssi = visible
                    .iter()
                    .filter(|a| a.ssid == n.ssid)
                    .map(|a| a.rssi as i32)
                    .max()
                    .unwrap_or(-1000);
                (rssi, n)
            })
            .collect();
        order.sort_by_key(|(rssi, _)| -rssi);
        let mut last = String::new();
        for (_, n) in order {
            match self.connect(&n.ssid, &n.password).await {
                Ok(ip) => return Ok(ip),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn handle(&mut self, req: Request) -> Response {
        match req {
            Request::Scan => Response::Scan(self.scan().await),
            Request::Connect { ssid, password } => {
                Response::Address(self.connect(&ssid, &password).await)
            }
            Request::Reconnect => Response::Address(self.reconnect().await),
            Request::Save(c) => {
                Response::Done(config::modify(|v| shell::store_config(v, &c)).map_err(String::from))
            }
            Request::FactoryReset => Response::Done(config::erase().map_err(String::from)),
        }
    }

    /// Joins a known network, then serves the shell.
    pub async fn run(mut self) -> ! {
        if shell::saved_config().is_some() {
            if self.reconnect().await.is_ok() {
                crate::control_probe(self.stack).await;
            }
        } else {
            shell::set_wifi_state("not configured".into());
        }
        loop {
            let req = shell::REQUESTS.receive().await;
            let resp = self.handle(req).await;
            shell::RESPONSES.send(resp).await;
        }
    }
}
