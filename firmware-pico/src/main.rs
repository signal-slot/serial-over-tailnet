//! serial-over-tailnet on the Raspberry Pi Pico 2 W.
//!
//! Core 0 runs the async side (USB console, Wi-Fi, network); core 1 runs
//! the setup shell, the same as on the ESP32 boards. The node is tsnode's
//! state machines driven by embassy tasks ([`node`]); the bridge carries
//! UART0 to a TCP port on the tailnet ([`bridge`]).

#![no_std]
#![no_main]

extern crate alloc;

mod bridge;
mod clock;
mod config;
mod console;
mod control;
mod node;
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
use embassy_rp::peripherals::{DMA_CH0, PIO0, UART0, USB};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, dma, uart, usb};
use embassy_time::{Duration, Timer};
use embedded_alloc::LlffHeap;
use log::{info, warn};
use static_cell::StaticCell;

#[global_allocator]
pub static HEAP: LlffHeap = LlffHeap::empty();
const HEAP_SIZE: usize = 320 * 1024;

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
    UART0_IRQ => uart::BufferedInterruptHandler<UART0>;
});

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
    rng::init(p.TRNG).await;
    // SAFETY: the stack is handed to core 1 once and never touched here.
    spawn_core1(
        p.CORE1,
        unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) },
        || shell::run(),
    );

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
    static RESOURCES: StaticCell<StackResources<12>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        u64::from_le_bytes(seed),
    );
    spawner.spawn(net_task(runner).unwrap());

    // The serial bridge: UART0 on GP0 (TX) and GP1 (RX).
    let saved = shell::saved_config();
    let mut uart_cfg = uart::Config::default();
    uart_cfg.baudrate = saved.as_ref().and_then(|c| c.baud_rate).unwrap_or(115_200);
    static UART_TX: StaticCell<[u8; 1024]> = StaticCell::new();
    static UART_RX: StaticCell<[u8; 2048]> = StaticCell::new();
    let uart = uart::BufferedUart::new(
        p.UART0,
        p.PIN_0,
        p.PIN_1,
        Irqs,
        UART_TX.init([0; 1024]),
        UART_RX.init([0; 2048]),
        uart_cfg,
    );
    let (tx, rx) = uart.split();
    let port = saved
        .as_ref()
        .and_then(|c| c.tcp_port)
        .unwrap_or(bridge::DEFAULT_PORT);
    spawner.spawn(bridge::bridge_task(tx, rx, port).unwrap());

    wifi::Wifi::new(control, stack, spawner).run().await
}
