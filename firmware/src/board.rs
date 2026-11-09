//! What the board around the module does. Two boards are supported:
//!
//! - the YD-ESP32-S3 development board (the default): nothing to drive, the
//!   native USB port is wired straight to the module;
//! - the adapter board in `hardware/board` (feature `board-adapter`): two
//!   USB-C ports that can both be the PC's or the target's. A TUSB320 per
//!   port tells the role the cable resolved to, an LM66200 picks the port
//!   that supplies the 5 V, a TS3USB221 sends the module's USB to one port
//!   and an AP22653 per port switches 5 V out to a target. In normal mode
//!   the firmware sends the USB to the target's port, fixes the roles of
//!   both detectors (the PC's port stays a sink, the target's a source) and
//!   turns the target port's 5 V on while something is attached there. In
//!   setup mode the USB stays at the PC's port, where the USB Serial/JTAG
//!   console is, which is also what the hardware does on its own before the
//!   firmware runs (the LM66200's status output sets the select through
//!   R11), and no 5 V goes out.

#[cfg(feature = "board-adapter")]
pub use adapter::{Board, Pins};
#[cfg(not(feature = "board-adapter"))]
pub use devkit::Board;

#[cfg(not(feature = "board-adapter"))]
mod devkit {
    pub struct Board;

    impl Board {
        pub fn init(_setup_mode: bool) -> anyhow::Result<Board> {
            Ok(Board)
        }

        /// Nothing to report: the board has no circuitry of its own.
        pub fn describe(&self) -> String {
            String::new()
        }
    }
}

#[cfg(feature = "board-adapter")]
mod adapter {
    use anyhow::{anyhow, Context, Result};
    use esp_idf_svc::hal::adc::oneshot::config::{AdcChannelConfig, Calibration, Resolution};
    use esp_idf_svc::hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
    use esp_idf_svc::hal::adc::{attenuation, ADC1};
    use esp_idf_svc::hal::delay::TickType;
    use esp_idf_svc::hal::gpio::{
        Gpio1, Gpio2, Gpio21, Gpio38, Gpio39, Gpio4, Gpio40, Gpio5, Gpio6, Gpio7, Gpio8, Gpio9,
        Input, Level, Output, Pin, PinDriver, Pull,
    };
    use esp_idf_svc::hal::i2c::{config::Config as I2cConfig, I2cDriver, I2C0};
    use esp_idf_svc::hal::units::Hertz;
    use esp_idf_svc::sys;
    use log::{info, warn};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The pins of `hardware/board/design.py`.
    pub struct Pins {
        /// TS3USB221 select: low sends the module's USB to port A, high to port B.
        pub mux_sel: Gpio21<'static>,
        /// LM66200 status: high while port B's VBUS supplies the 5 V.
        pub pwr_st: Gpio7<'static>,
        pub vbus_en_a: Gpio6<'static>,
        pub vbus_en_b: Gpio40<'static>,
        pub fault_a: Gpio5<'static>,
        pub fault_b: Gpio39<'static>,
        pub id_a: Gpio4<'static>,
        pub id_b: Gpio38<'static>,
        pub led: Gpio2<'static>,
        /// Half of VREF, the UART header's voltage (R22 and R23 halve it).
        pub vref_sense: Gpio1<'static>,
        pub adc1: ADC1<'static>,
        pub i2c0: I2C0<'static>,
        pub sda: Gpio8<'static>,
        pub scl: Gpio9<'static>,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Port {
        A,
        B,
    }

    impl Port {
        fn other(self) -> Port {
            match self {
                Port::A => Port::B,
                Port::B => Port::A,
            }
        }
    }

    /// TUSB320LAI over I2C: ADDR low (port A) is 0x47, high (port B) 0x67.
    const TUSB320_ADDR: [u8; 2] = [0x47, 0x67];
    const TUSB320_CSR_CURRENT: u8 = 0x08; // CURRENT_MODE_DETECT in bits 5:4
    const TUSB320_CSR_MODE: u8 = 0x0A; // MODE_SELECT in bits 5:4, DISABLE_TERM bit 0
    const MODE_UFP: u8 = 0b01;
    const MODE_DFP: u8 = 0b10;
    const I2C_TIMEOUT_MS: u64 = 50;

    /// The outputs are held so that they keep their level (dropping a
    /// PinDriver resets the pin).
    struct Lines {
        #[allow(dead_code)]
        mux: PinDriver<'static, Output>,
        vbus_a: PinDriver<'static, Output>,
        vbus_b: PinDriver<'static, Output>,
        pwr_st: PinDriver<'static, Input>,
        fault_a: PinDriver<'static, Input>,
        fault_b: PinDriver<'static, Input>,
        id_a: PinDriver<'static, Input>,
        id_b: PinDriver<'static, Input>,
        #[allow(dead_code)]
        led: PinDriver<'static, Output>,
        vref: VrefSense,
        i2c: I2cDriver<'static>,
    }

    /// Reads VREF in millivolts. The ADC handles are raw pointers, which
    /// makes the drivers `!Send`; they are only ever used under the board's
    /// lock, from one thread at a time.
    struct VrefSense(Box<dyn FnMut() -> Option<u32>>);
    unsafe impl Send for VrefSense {}

    /// What the monitor last saw and did.
    #[derive(Clone, Copy, Default)]
    struct State {
        /// The target port's 5 V output is on.
        vbus_out: bool,
        /// The PC's port advertises only the default current (500 mA from a
        /// Type-A port): the module and a target together can exceed it.
        low_current: bool,
    }

    pub struct Board {
        /// The port that supplies the 5 V: the PC's.
        powered_by: Port,
        /// The port the module's USB and the 5 V output go to; none in setup mode.
        target: Option<Port>,
        lines: Arc<Mutex<Lines>>,
        state: Arc<Mutex<State>>,
    }

    impl Board {
        pub fn init(pins: Pins, setup_mode: bool) -> Result<Board> {
            // GPIO39 and GPIO40 come out of reset as JTAG pins (MTCK, MTDO):
            // without selecting the GPIO function at the pad, driving them
            // moves nothing. Done for every pin, which costs nothing.
            for pin in [
                pins.mux_sel.pin(),
                pins.pwr_st.pin(),
                pins.vbus_en_a.pin(),
                pins.vbus_en_b.pin(),
                pins.fault_a.pin(),
                pins.fault_b.pin(),
                pins.id_a.pin(),
                pins.id_b.pin(),
                pins.led.pin(),
            ] {
                unsafe { sys::esp_rom_gpio_pad_select_gpio(pin as u32) };
            }
            let pwr_st = PinDriver::input(pins.pwr_st, Pull::Floating)?;
            std::thread::sleep(Duration::from_millis(2));
            let powered_by = if pwr_st.is_high() { Port::B } else { Port::A };
            let target = (!setup_mode).then(|| powered_by.other());
            let usb_at = target.unwrap_or(powered_by);
            // The level is set before the output is enabled, so that the
            // select never passes through the other port on the way (a USB
            // disconnect is seen after 2.5 µs of SE0).
            unsafe { sys::gpio_set_level(pins.mux_sel.pin() as i32, (usb_at == Port::B) as u32) };
            let mux = PinDriver::output(pins.mux_sel)?;
            unsafe {
                sys::gpio_set_level(pins.vbus_en_a.pin() as i32, 0);
                sys::gpio_set_level(pins.vbus_en_b.pin() as i32, 0);
            }
            let vbus_a = PinDriver::output(pins.vbus_en_a)?;
            let vbus_b = PinDriver::output(pins.vbus_en_b)?;
            let mut led = PinDriver::output(pins.led)?;
            led.set_high()?;
            let i2c = I2cDriver::new(
                pins.i2c0,
                pins.sda,
                pins.scl,
                &I2cConfig::new().baudrate(Hertz(100_000)),
            )
            .context("i2c")?;
            let mut lines = Lines {
                mux,
                vbus_a,
                vbus_b,
                pwr_st,
                // All four have their pull-ups on the board.
                fault_a: PinDriver::input(pins.fault_a, Pull::Floating)?,
                fault_b: PinDriver::input(pins.fault_b, Pull::Floating)?,
                id_a: PinDriver::input(pins.id_a, Pull::Floating)?,
                id_b: PinDriver::input(pins.id_b, Pull::Floating)?,
                led,
                vref: vref_sense(pins.adc1, pins.vref_sense)?,
                i2c,
            };
            let mut state = State::default();
            if let Some(target) = target {
                // The PC's port stays a sink, the target's becomes a source:
                // two role-toggling ports would resolve at random against a
                // dual-role PC or target.
                for (port, mode) in [(powered_by, MODE_UFP), (target, MODE_DFP)] {
                    if let Err(e) = set_mode(&mut lines.i2c, port, mode) {
                        warn!("board: TUSB320 of port {port:?}: {e:#}");
                    }
                }
                match read(&mut lines.i2c, powered_by, TUSB320_CSR_CURRENT) {
                    Ok(csr) => {
                        state.low_current = (csr >> 4) & 0b11 == 0;
                        if state.low_current {
                            warn!(
                                "board: port {powered_by:?} advertises only the default current (500 mA); \
                                 the module and a target drawing more than ~150 mA together exceed it"
                            );
                        }
                    }
                    Err(e) => warn!("board: TUSB320 of port {powered_by:?}: {e:#}"),
                }
            }
            let board = Board {
                powered_by,
                target,
                lines: Arc::new(Mutex::new(lines)),
                state: Arc::new(Mutex::new(state)),
            };
            info!("board: {}", board.describe());
            if let Some(target) = target {
                board.spawn_monitor(target)?;
            }
            Ok(board)
        }

        /// Turns the target port's 5 V on while the detector sees a sink
        /// attached there (its ID output low), and off when it leaves: the
        /// Type-C way, and it keeps 5 V out of a port that carries its own.
        fn spawn_monitor(&self, target: Port) -> Result<()> {
            let lines = self.lines.clone();
            let state = self.state.clone();
            crate::platform::spawn_thread(
                "board",
                4 * 1024,
                false,
                Box::new(move || loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let mut l = lines.lock().unwrap();
                    let Lines {
                        id_a,
                        id_b,
                        vbus_a,
                        vbus_b,
                        fault_a,
                        fault_b,
                        ..
                    } = &mut *l;
                    let (id, vbus, fault) = match target {
                        Port::A => (id_a, vbus_a, fault_a),
                        Port::B => (id_b, vbus_b, fault_b),
                    };
                    let attached = id.is_low();
                    let mut s = state.lock().unwrap();
                    if attached != s.vbus_out {
                        if let Err(e) = vbus.set_level(level(attached)) {
                            warn!("board: VBUS_EN of port {target:?}: {e}");
                            continue;
                        }
                        s.vbus_out = attached;
                        info!(
                            "board: port {target:?}: {}",
                            if attached { "sink attached, 5 V out on" } else { "detached, 5 V out off" }
                        );
                    } else if attached && fault.is_low() {
                        warn!("board: port {target:?}: 5 V switch reports a fault (over-current or hot)");
                    }
                }),
            )
            .map_err(|e| anyhow!("{e}"))
        }

        /// Which port powers the board, where the USB goes, and what the
        /// role detectors, the 5 V switches and the VREF sense report.
        pub fn describe(&self) -> String {
            let mut l = self.lines.lock().unwrap();
            let s = *self.state.lock().unwrap();
            let vref = match (l.vref.0)() {
                Some(mv) => format!("{}.{:02} V", mv / 1000, (mv % 1000) / 10),
                None => "unreadable".into(),
            };
            // TUSB320's ID is low while it is the source (DFP); AP22653's
            // FAULT is low while it limits the current or is too hot.
            let id =
                |p: &PinDriver<'static, Input>| if p.is_low() { "source" } else { "sink/none" };
            let fault = |p: &PinDriver<'static, Input>| if p.is_low() { ", FAULT" } else { "" };
            let powered_now = if l.pwr_st.is_high() { Port::B } else { Port::A };
            format!(
                "powered by port {:?}{}; {}; role A={}{} B={}{}; VREF {vref}",
                self.powered_by,
                if powered_now != self.powered_by {
                    format!(" at boot, now {powered_now:?} (reboot to follow)")
                } else {
                    String::new()
                },
                match self.target {
                    Some(t) => format!(
                        "USB to port {t:?}, 5 V out {}{}",
                        if s.vbus_out {
                            "on"
                        } else {
                            "off (nothing attached)"
                        },
                        if s.low_current {
                            ", PC port advertises 500 mA"
                        } else {
                            ""
                        }
                    ),
                    None => format!(
                        "USB to port {:?} (setup console), 5 V out off",
                        self.powered_by
                    ),
                },
                id(&l.id_a),
                fault(&l.fault_a),
                id(&l.id_b),
                fault(&l.fault_b),
            )
        }
    }

    fn read(i2c: &mut I2cDriver<'static>, port: Port, csr: u8) -> Result<u8> {
        let mut byte = [0u8];
        i2c.write_read(TUSB320_ADDR[port as usize], &[csr], &mut byte, timeout())
            .with_context(|| format!("read CSR {csr:#04x}"))?;
        Ok(byte[0])
    }

    fn write(i2c: &mut I2cDriver<'static>, port: Port, csr: u8, value: u8) -> Result<()> {
        i2c.write(TUSB320_ADDR[port as usize], &[csr, value], timeout())
            .with_context(|| format!("write CSR {csr:#04x}"))
    }

    /// The datasheet's sequence for changing the mode while powered: disable
    /// the CC terminations, write MODE_SELECT, wait 5 ms, enable them again.
    fn set_mode(i2c: &mut I2cDriver<'static>, port: Port, mode: u8) -> Result<()> {
        let csr = read(i2c, port, TUSB320_CSR_MODE)?;
        write(i2c, port, TUSB320_CSR_MODE, csr | 0x01)?;
        write(
            i2c,
            port,
            TUSB320_CSR_MODE,
            (csr & !0x30) | (mode << 4) | 0x01,
        )?;
        std::thread::sleep(Duration::from_millis(5));
        write(i2c, port, TUSB320_CSR_MODE, (csr & !0x30) | (mode << 4))?;
        Ok(())
    }

    fn timeout() -> u32 {
        TickType::from(Duration::from_millis(I2C_TIMEOUT_MS)).0
    }

    fn vref_sense(adc1: ADC1<'static>, pin: Gpio1<'static>) -> Result<VrefSense> {
        let adc = AdcDriver::new(adc1)?;
        let config = AdcChannelConfig {
            attenuation: attenuation::DB_12,
            resolution: Resolution::Resolution12Bit,
            calibration: Calibration::Curve,
        };
        let mut channel = AdcChannelDriver::new(adc, pin, &config)?;
        // The divider's 50 kΩ source reads a little low on one sample;
        // eight are averaged.
        Ok(VrefSense(Box::new(move || {
            let mut sum = 0u32;
            for _ in 0..8 {
                sum += channel.read().ok()? as u32;
            }
            Some(2 * sum / 8)
        })))
    }

    fn level(on: bool) -> Level {
        if on {
            Level::High
        } else {
            Level::Low
        }
    }
}
