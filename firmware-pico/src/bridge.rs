//! The serial bridge: a TCP port on the tailnet (35930 unless set) carries
//! the bytes of UART0 (GP0 TX, GP1 RX, 3.3 V), as on the XIAO boards. One
//! client at a time; a second one is told `busy` and closed.

use crate::node;
use alloc::vec::Vec;
use embassy_futures::select::{select3, Either3};
use embassy_rp::uart::{BufferedUartRx, BufferedUartTx};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use embedded_io_async::{Read, Write};
use log::info;
use tsnode::netstack::ConnId;

pub const DEFAULT_PORT: u16 = 35930;
/// UART bytes held for the network at most.
const PENDING_MAX: usize = 4096;

static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The node's stack changed: look at the connections again.
pub fn wake() {
    WAKE.signal(());
}

#[embassy_executor::task]
pub async fn bridge_task(mut tx: BufferedUartTx, mut rx: BufferedUartRx, port: u16) -> ! {
    // Listen once the node runs.
    let listener = loop {
        if let Some(Some(l)) = node::with_net(|n| n.listen(port)) {
            break l;
        }
        Timer::after(Duration::from_secs(1)).await;
    };
    info!("bridge: UART0 on port {port}");
    let mut conn: Option<ConnId> = None;
    // Clients on their way out (a turned-away one with whether it has been
    // told `busy`), until their connection has closed.
    let mut leaving: Vec<(ConnId, bool)> = Vec::new();
    // Bytes from the UART the stack had no room for yet. Past PENDING_MAX
    // the UART is not read (its own buffer then fills, as on a cable
    // without flow control).
    let mut pending: Vec<u8> = Vec::new();
    // Bytes from the client on their way to the UART: net_buf[out..out_end].
    let mut net_buf = [0u8; 512];
    let (mut out, mut out_end) = (0usize, 0usize);
    let mut uart_buf = [0u8; 512];
    loop {
        node::with_net(|n| {
            while let Some((c, peer)) = n.accept(listener) {
                if conn.is_none() {
                    info!("bridge: client {peer}");
                    conn = Some(c);
                } else {
                    info!("bridge: {peer} turned away (busy)");
                    leaving.push((c, false));
                }
            }
            leaving.retain_mut(|(c, told)| {
                let st = n.status(*c);
                // The stack takes data only once the handshake is done.
                if !*told && !st.connecting {
                    n.send(*c, b"busy\r\n");
                    n.close(*c);
                    *told = true;
                }
                if st.closed {
                    n.remove(*c);
                }
                !st.closed
            });
            if let Some(c) = conn {
                if out == out_end && n.can_recv(c) {
                    out = 0;
                    out_end = n.recv(c, &mut net_buf);
                }
                if !pending.is_empty() {
                    let k = n.send(c, &pending);
                    pending.drain(..k);
                }
                let st = n.status(c);
                // A connection is accepted before its handshake ends, and
                // until then `may_recv` is false too.
                if st.closed || (!st.may_recv && !st.connecting && out == out_end) {
                    info!("bridge: client gone");
                    n.close(c);
                    if st.closed {
                        n.remove(c);
                    } else {
                        leaving.push((c, true));
                    }
                    conn = None;
                    pending.clear();
                }
            }
        });
        // The UART is read and written at once: a loop that waited for one
        // lost received bytes while it wrote (seen with GP0 tied to GP1).
        let read = async {
            if pending.len() < PENDING_MAX {
                rx.read(&mut uart_buf).await
            } else {
                core::future::pending().await
            }
        };
        let write = async {
            if out < out_end {
                tx.write(&net_buf[out..out_end]).await
            } else {
                core::future::pending().await
            }
        };
        match select3(WAKE.wait(), read, write).await {
            Either3::First(()) => {}
            // Nobody listening: the bytes go nowhere, as on a cable.
            Either3::Second(Ok(k)) if conn.is_some() => pending.extend_from_slice(&uart_buf[..k]),
            Either3::Second(_) => {}
            Either3::Third(Ok(k)) => out += k,
            Either3::Third(Err(_)) => out = out_end,
        }
    }
}
