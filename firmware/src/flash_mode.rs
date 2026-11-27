//! Keeps the app's flash reads in the mode the bootloader could set up.
//!
//! The images are built for QIO. When the bootloader cannot set the flash
//! chip's quad-enable bit (seen on a YD-ESP32-S3 N8R2 with an EON flash,
//! JEDEC ID 1c 3017: "qio_mode: Failed to set QIE bit, not enabling QIO
//! mode"), it runs the cache in DIO, but ESP-IDF's flash driver still reads
//! in QIO: its own quad-enable check passes on that chip. Those reads come
//! back wrong, and NVS writes the wrong bits back into its entry tables:
//! saved settings vanished at the next boot.
//!
//! The cache's mode is the one known to work, since the code runs from it,
//! so the driver is given the same mode before anything touches NVS.

use esp_idf_svc::sys::{
    esp_flash_default_chip, esp_flash_io_mode_t, esp_flash_io_mode_t_SPI_FLASH_DIO,
    esp_flash_io_mode_t_SPI_FLASH_DOUT, esp_flash_io_mode_t_SPI_FLASH_FASTRD,
    esp_flash_io_mode_t_SPI_FLASH_QIO, esp_flash_io_mode_t_SPI_FLASH_QOUT,
};
use log::warn;

/// SPI_MEM_CTRL_REG(0), the cache's flash controller (same address and bits
/// on the ESP32-S3 and the ESP32-C3).
const SPI0_CTRL: usize = 0x6000_3008;
const FREAD_DUAL: u32 = 1 << 14;
const FREAD_QUAD: u32 = 1 << 20;
const FREAD_DIO: u32 = 1 << 23;
const FREAD_QIO: u32 = 1 << 24;

fn cache_mode() -> esp_flash_io_mode_t {
    let ctrl = unsafe { core::ptr::read_volatile(SPI0_CTRL as *const u32) };
    if ctrl & FREAD_QIO != 0 {
        esp_flash_io_mode_t_SPI_FLASH_QIO
    } else if ctrl & FREAD_QUAD != 0 {
        esp_flash_io_mode_t_SPI_FLASH_QOUT
    } else if ctrl & FREAD_DIO != 0 {
        esp_flash_io_mode_t_SPI_FLASH_DIO
    } else if ctrl & FREAD_DUAL != 0 {
        esp_flash_io_mode_t_SPI_FLASH_DOUT
    } else {
        esp_flash_io_mode_t_SPI_FLASH_FASTRD
    }
}

fn is_quad(m: esp_flash_io_mode_t) -> bool {
    m == esp_flash_io_mode_t_SPI_FLASH_QIO || m == esp_flash_io_mode_t_SPI_FLASH_QOUT
}

/// Call before the NVS partition is taken.
pub fn match_bootloader() {
    let chip = unsafe { esp_flash_default_chip };
    if chip.is_null() {
        return;
    }
    let cache = cache_mode();
    let driver = unsafe { (*chip).read_mode };
    if is_quad(driver) && !is_quad(cache) {
        warn!(
            "flash: the bootloader could not enable quad mode; reading flash in its mode instead"
        );
        unsafe { (*chip).read_mode = cache };
    }
}
