//! serial-over-tailnet on the Raspberry Pi Pico 2 W.
//!
//! Core 0 runs the async side (USB console, Wi-Fi, network); core 1 runs
//! the setup shell, the same as on the ESP32 boards. This first stage has
//! the shell, Wi-Fi and the stored settings; the node itself (tsnode's state
//! machines on embassy) comes next.

#![no_std]
#![no_main]

extern crate alloc;

mod clock;
mod config;
mod console;
mod control;
mod panic;
mod rng;
mod shell;
mod tls;
mod wifi;

use core::mem::MaybeUninit;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, StackResources};
use embassy_rp::flash::Flash;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::{spawn_core1, Stack as CoreStack};
use embassy_rp::peripherals::{DMA_CH0, PIO0, TRNG, USB};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, dma, trng, usb};
use embassy_time::{Duration, Timer};
use embedded_alloc::LlffHeap;
use log::{info, warn};
use static_cell::StaticCell;

#[global_allocator]
pub static HEAP: LlffHeap = LlffHeap::empty();
const HEAP_SIZE: usize = 256 * 1024;

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 3] = [
    embassy_rp::binary_info::rp_program_name!(c"serial-over-tailnet"),
    embassy_rp::binary_info::rp_program_description!(
        c"Bridges a serial port to a Tailscale tailnet (Raspberry Pi Pico 2 W)"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    TRNG_IRQ => trng::InterruptHandler<TRNG>;
});

/// The test of the control path until the node runs here: registers a
/// throwaway key and logs the answer.
pub async fn control_probe(stack: embassy_net::Stack<'static>) {
    match control::probe(stack).await {
        Ok(p) => info!(
            "control: register OK (authorized {}, auth URL {}, error '{}')",
            p.authorized,
            if p.auth_url.is_empty() {
                "none"
            } else {
                "given"
            },
            p.error
        ),
        Err(e) => warn!("control: probe failed: {e}"),
    }
}

static mut CORE1_STACK: CoreStack<{ 32 * 1024 }> = CoreStack::new();

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::main(
    executor = "embassy_rp::executor::Executor",
    entry = "cortex_m_rt::entry"
)]
async fn main(spawner: Spawner) {
    {
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        // SAFETY: runs once, before anything allocates.
        unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
    }
    let p = embassy_rp::init(Default::default());
    let previous_panic = panic::take();
    console::init_logger();
    spawner.spawn(console::usb_task(usb::Driver::new(p.USB, Irqs)).unwrap());
    config::init(Flash::new_blocking(p.FLASH));

    // Give the host a moment to open the console before the first lines.
    Timer::after(Duration::from_secs(2)).await;
    info!(
        "serial-over-tailnet {} on the Pico 2 W",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(msg) = previous_panic {
        warn!("the last run ended in a panic: {msg}");
        shell::set_last_reset(alloc::format!("panic: {msg}"));
    }
    // SAFETY: the stack is handed to core 1 once and never touched here.
    spawn_core1(
        p.CORE1,
        unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) },
        || shell::run(),
    );
    let mut trng = trng::Trng::new(p.TRNG, Irqs, trng::Config::default());
    rng::init(&mut trng).await;

    let fw = cyw43::aligned_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = cyw43::aligned_bytes!("../cyw43-firmware/43439A0_clm.bin");
    let nvram = cyw43::aligned_bytes!("../cyw43-firmware/nvram_rp2040.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) =
        cyw43::new(STATE.init(cyw43::State::new()), pwr, spi, fw, nvram).await;
    spawner.spawn(cyw43_task(runner).unwrap());
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;
    info!("wifi: chip ready");

    let mut seed = [0u8; 8];
    let _ = getrandom::getrandom(&mut seed);
    static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        u64::from_le_bytes(seed),
    );
    spawner.spawn(net_task(runner).unwrap());

    wifi::Wifi::new(control, stack).run().await
}
