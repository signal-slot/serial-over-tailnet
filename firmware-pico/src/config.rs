//! Settings and node identity, kept in the last 64 KiB of flash (outside the
//! program, see memory.x) as `key=value` lines.
//!
//! One 4 KiB sector holds everything: a magic, the length, the lines and a
//! checksum. A write erases the sector and writes it again; writes are rare
//! (setup, registration, a new home DERP), so wear does not matter.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use critical_section::Mutex;
use embassy_rp::flash::{Blocking, Flash, ERASE_SIZE};
use embassy_rp::peripherals::FLASH;

pub const FLASH_SIZE: usize = 4 * 1024 * 1024;
/// Offset of the configuration sector from the start of flash.
const OFFSET: u32 = (FLASH_SIZE - 64 * 1024) as u32;
const MAGIC: &[u8; 4] = b"SOTC";

type Dev = Flash<'static, FLASH, Blocking, FLASH_SIZE>;

static FLASH_DEV: Mutex<RefCell<Option<Dev>>> = Mutex::new(RefCell::new(None));
static VALUES: Mutex<RefCell<BTreeMap<String, String>>> = Mutex::new(RefCell::new(BTreeMap::new()));

fn checksum(data: &[u8]) -> u32 {
    // FNV-1a: catches a torn or erased sector, which is all it is for.
    data.iter().fold(0x811c_9dc5u32, |h, &b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

/// Takes the flash and reads the stored values.
pub fn init(mut flash: Dev) {
    let mut sector = alloc::vec![0u8; ERASE_SIZE];
    let values = match flash.blocking_read(OFFSET, &mut sector) {
        Ok(()) => parse(&sector).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    };
    critical_section::with(|cs| {
        *FLASH_DEV.borrow_ref_mut(cs) = Some(flash);
        *VALUES.borrow_ref_mut(cs) = values;
    });
}

fn parse(sector: &[u8]) -> Option<BTreeMap<String, String>> {
    if &sector[..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(sector[4..8].try_into().ok()?) as usize;
    if len > ERASE_SIZE - 12 {
        return None;
    }
    let body = &sector[8..8 + len];
    let sum = u32::from_le_bytes(sector[8 + len..12 + len].try_into().ok()?);
    if sum != checksum(body) {
        return None;
    }
    let text = core::str::from_utf8(body).ok()?;
    Some(
        text.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

/// A copy of all stored values.
pub fn values() -> BTreeMap<String, String> {
    critical_section::with(|cs| VALUES.borrow_ref(cs).clone())
}

/// Changes the values with `f` and writes them to flash. Call it on core 0:
/// writing flash stops execution from it, which core 1 must not be doing.
pub fn modify(f: impl FnOnce(&mut BTreeMap<String, String>)) -> Result<(), &'static str> {
    let mut values = values();
    f(&mut values);
    let mut body = String::new();
    for (k, v) in values.iter() {
        if k.contains(['=', '\n']) || v.contains('\n') {
            return Err("bad character in a value");
        }
        body.push_str(k);
        body.push('=');
        body.push_str(v);
        body.push('\n');
    }
    if body.len() > ERASE_SIZE - 12 {
        return Err("configuration too large");
    }
    let mut sector: Vec<u8> = Vec::with_capacity(body.len() + 12);
    sector.extend_from_slice(MAGIC);
    sector.extend_from_slice(&(body.len() as u32).to_le_bytes());
    sector.extend_from_slice(body.as_bytes());
    sector.extend_from_slice(&checksum(body.as_bytes()).to_le_bytes());
    critical_section::with(|cs| {
        let mut dev = FLASH_DEV.borrow_ref_mut(cs);
        let flash = dev.as_mut().ok_or("flash not ready")?;
        flash
            .blocking_erase(OFFSET, OFFSET + ERASE_SIZE as u32)
            .map_err(|_| "flash erase failed")?;
        flash
            .blocking_write(OFFSET, &sector)
            .map_err(|_| "flash write failed")?;
        *VALUES.borrow_ref_mut(cs) = values;
        Ok(())
    })
}

/// Forgets everything (Wi-Fi, auth key, node identity).
pub fn erase() -> Result<(), &'static str> {
    critical_section::with(|cs| {
        VALUES.borrow_ref_mut(cs).clear();
        let mut dev = FLASH_DEV.borrow_ref_mut(cs);
        let flash = dev.as_mut().ok_or("flash not ready")?;
        flash
            .blocking_erase(OFFSET, OFFSET + ERASE_SIZE as u32)
            .map_err(|_| "flash erase failed")
    })
}
