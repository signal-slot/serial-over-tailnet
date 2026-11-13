//! A UART of the module as a channel for `bridge` (UART1; the pins are the
//! board's, see `main`):
//!
//! - the adapter board's UART header (`hardware/board`: G, TX, RX, V), IO33
//!   (TX) and IO34 (RX) through two level translators supplied from the
//!   header's V pin or the board's 3.3 V (the VREF switch), so the target
//!   can be a 1.8 V or a 3.3 V one;
//! - the XIAO ESP32S3's pins D4 (TX) and D5 (RX), wired straight to the
//!   target: 3.3 V only.
//!
//! There are no modem lines on the header: DTR and RTS requests are accepted
//! and do nothing. A break is made by inverting TX while it lasts.

use crate::bridge::Serial;
use crate::usb_host::OpenError;
use adapter_core::usbserial::{LineSettings, Parity, StopBits};
use anyhow::{anyhow, Result};
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{InputPin, OutputPin};
use esp_idf_svc::hal::uart::{config, UartDriver, UART1};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct UartSerial {
    inner: Arc<Inner>,
}

struct Inner {
    uart: UartDriver<'static>,
    /// The baud rate every session starts at (the `baud` setting).
    baud: u32,
    line: Mutex<LineSettings>,
    modem: Mutex<(bool, bool)>,
}

impl UartSerial {
    pub fn new(
        uart1: UART1<'static>,
        tx: impl OutputPin + 'static,
        rx: impl InputPin + 'static,
        baud: u32,
    ) -> Result<UartSerial> {
        let cfg = config::Config::default()
            .baudrate(Hertz(baud))
            .rx_fifo_size(2048)
            .tx_fifo_size(1024);
        let uart = UartDriver::new(
            uart1,
            tx,
            rx,
            Option::<esp_idf_svc::hal::gpio::AnyIOPin>::None,
            Option::<esp_idf_svc::hal::gpio::AnyIOPin>::None,
            &cfg,
        )?;
        Ok(UartSerial {
            inner: Arc::new(Inner {
                uart,
                baud,
                line: Mutex::new(LineSettings::new(baud)),
                modem: Mutex::new((true, true)),
            }),
        })
    }

    /// Lets what is queued go out at the current settings: the TX ring holds
    /// up to 1024 bytes, a second at 9600 baud.
    fn drain(&self) {
        let _ = self
            .inner
            .uart
            .wait_tx_done(TickType::from(Duration::from_secs(3)).0);
    }

    fn apply(&self, line: &LineSettings) -> Result<()> {
        let u = &self.inner.uart;
        self.drain();
        u.change_baudrate(Hertz(line.baud))?;
        u.change_data_bits(match line.data_bits {
            5 => config::DataBits::DataBits5,
            6 => config::DataBits::DataBits6,
            7 => config::DataBits::DataBits7,
            8 => config::DataBits::DataBits8,
            n => return Err(anyhow!("{n} data bits is not a UART setting")),
        })?;
        u.change_parity(match line.parity {
            Parity::None => config::Parity::ParityNone,
            Parity::Odd => config::Parity::ParityOdd,
            Parity::Even => config::Parity::ParityEven,
            Parity::Mark | Parity::Space => {
                return Err(anyhow!("the UART has no mark or space parity"))
            }
        })?;
        u.change_stop_bits(match line.stop_bits {
            StopBits::One => config::StopBits::STOP1,
            StopBits::OneAndHalf => config::StopBits::STOP1P5,
            StopBits::Two => config::StopBits::STOP2,
        })?;
        Ok(())
    }
}

impl Serial for UartSerial {
    /// The header is always there: a session starts at the configured baud
    /// rate, 8N1, with whatever the target sent before dropped.
    fn open(&self) -> std::result::Result<(), OpenError> {
        let line = LineSettings::new(self.inner.baud);
        self.apply(&line)
            .map_err(|e| OpenError::Failed(e.to_string()))?;
        *self.inner.line.lock().unwrap() = line;
        *self.inner.modem.lock().unwrap() = (true, true);
        let _ = self.inner.uart.clear_rx();
        Ok(())
    }

    fn close(&self) {
        let _ = self.set_break(false);
    }

    fn line(&self) -> LineSettings {
        *self.inner.line.lock().unwrap()
    }

    fn set_line(&self, line: LineSettings) -> Result<()> {
        self.apply(&line)?;
        *self.inner.line.lock().unwrap() = line;
        Ok(())
    }

    fn modem(&self) -> (bool, bool) {
        *self.inner.modem.lock().unwrap()
    }

    /// Remembered so that the session reads back what it set; the header has
    /// no such lines.
    fn set_modem(&self, dtr: bool, rts: bool) -> Result<()> {
        *self.inner.modem.lock().unwrap() = (dtr, rts);
        Ok(())
    }

    fn set_break(&self, on: bool) -> Result<()> {
        self.drain();
        let mask = if on {
            sys::uart_signal_inv_t_UART_SIGNAL_TXD_INV
        } else {
            sys::uart_signal_inv_t_UART_SIGNAL_INV_DISABLE
        };
        let rc = unsafe { sys::uart_set_line_inverse(self.inner.uart.port(), mask) };
        if rc != sys::ESP_OK {
            return Err(anyhow!("uart_set_line_inverse failed ({rc})"));
        }
        Ok(())
    }

    fn purge_rx(&self) {
        let _ = self.inner.uart.clear_rx();
    }

    fn read(&self, buf: &mut [u8], timeout: Duration) -> usize {
        let ticks: TickType = timeout.into();
        self.inner.uart.read(buf, ticks.ticks()).unwrap_or(0)
    }

    fn write(&self, data: &[u8]) -> Result<()> {
        let mut rest = data;
        while !rest.is_empty() {
            let n = self.inner.uart.write(rest)?;
            rest = &rest[n..];
        }
        Ok(())
    }
}
