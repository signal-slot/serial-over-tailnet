//! The USB console: one CDC-ACM serial port that carries the log and the
//! setup shell's input and output (the shell, adapter-core's, does its own
//! line editing and echo).
//!
//! Opening the port at 1200 baud and closing it reboots into the BOOTSEL
//! loader, so a new image can be loaded without pressing the button.

use alloc::string::String;
use core::fmt::Write as _;
use embassy_futures::join::join3;
use embassy_futures::select::{select, Either};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::{Builder, Config};
use static_cell::StaticCell;

/// Bytes waiting for the host; when it is full (no host reading), new
/// output is dropped.
static OUT: Pipe<CriticalSectionRawMutex, 8192> = Pipe::new();
/// Bytes typed by the user, for the shell.
static IN: Pipe<CriticalSectionRawMutex, 256> = Pipe::new();
/// Writes `s` to the console as it is.
pub fn print(s: &str) {
    // A full pipe takes what fits; the rest is dropped.
    let _ = OUT.try_write(s.as_bytes());
}

/// The next byte typed, if one is waiting (callable from either core).
pub fn try_read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    match IN.try_read(&mut b) {
        Ok(1) => Some(b[0]),
        _ => None,
    }
}

struct Logger;

static LOGGER: Logger = Logger;

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let ms = embassy_time::Instant::now().as_millis();
        let mut s = String::new();
        let _ = write!(
            s,
            "{}.{:03} {} {}: {}\r\n",
            ms / 1000,
            ms % 1000,
            r.level(),
            r.target(),
            r.args()
        );
        print(&s);
    }

    fn flush(&self) {}
}

pub fn init_logger() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

#[embassy_executor::task]
pub async fn usb_task(driver: Driver<'static, USB>) -> ! {
    let mut config = Config::new(0x2e8a, 0x000a);
    config.manufacturer = Some("signal-slot");
    config.product = Some("serial-over-tailnet");
    config.serial_number = None;
    config.max_power = 250;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );
    let class = CdcAcmClass::new(&mut builder, STATE.init(State::new()), 64);
    let mut usb = builder.build();
    let (mut tx, mut rx, ctrl) = class.split_with_control();

    let writer = async {
        let mut buf = [0u8; 64];
        loop {
            tx.wait_connection().await;
            // Send only to a host that has the port open (DTR up). Something
            // that opens it briefly without DTR (a port scanner) would
            // otherwise take the output, and its terminal could echo it back
            // to us as typing.
            while !tx.dtr() {
                embassy_time::Timer::after_millis(50).await;
            }
            loop {
                let n = OUT.read(&mut buf).await;
                if !tx.dtr() || tx.write_packet(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    };

    let reader = async {
        let mut buf = [0u8; 64];
        loop {
            match select(rx.read_packet(&mut buf), ctrl.control_changed()).await {
                Either::First(Ok(n)) => {
                    let _ = IN.try_write(&buf[..n]);
                }
                Either::First(Err(_)) => rx.wait_connection().await,
                Either::Second(()) => {
                    if rx.line_coding().data_rate() == 1200 && !ctrl.dtr() {
                        embassy_rp::rom_data::reset_to_usb_boot(0, 0);
                    }
                }
            }
        }
    };

    join3(usb.run(), writer, reader).await;
    unreachable!()
}
