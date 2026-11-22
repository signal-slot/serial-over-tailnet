//! Settings and node identity, kept in the last 64 KiB of flash (outside the
//! program, see memory.x) as `key=value` lines.
//!
//! Two 4 KiB sectors are written in turn. Each copy holds a magic, a
//! generation number, the length, the lines and a checksum; the valid copy
//! with the higher generation is the current one. A write goes to the other
//! sector, so a write cut short (power, a reset) leaves the previous
//! settings. Writes are rare (setup, registration, a new home DERP).
//!
//! The flash is written from core 0 only, and never inside a critical
//! section: embassy-rp pauses core 1 for the write and waits for it to
//! answer, which it cannot do while it waits for the critical section's
//! lock (both cores stopped, seen when the node stored its keys).

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use critical_section::Mutex;
use embassy_rp::flash::{Blocking, Flash, ERASE_SIZE};
use embassy_rp::peripherals::FLASH;

pub const FLASH_SIZE: usize = 4 * 1024 * 1024;
/// Offsets of the two configuration sectors from the start of flash.
const SECTORS: [u32; 2] = [
    (FLASH_SIZE - 64 * 1024) as u32,
    (FLASH_SIZE - 64 * 1024 + ERASE_SIZE) as u32,
];
const MAGIC: &[u8; 4] = b"SOT2";
/// Magic, generation, length; the checksum follows the body.
const HEADER: usize = 12;

type Dev = Flash<'static, FLASH, Blocking, FLASH_SIZE>;

/// The flash and which sector holds the current copy (with its generation).
struct Store {
    flash: Dev,
    current: Option<(usize, u32)>,
}

/// Only core 0 touches it (see the module documentation).
struct Core0<T>(RefCell<Option<T>>);
// SAFETY: reached only from core 0, from its single-threaded executor.
unsafe impl<T> Sync for Core0<T> {}

static STORE: Core0<Store> = Core0(RefCell::new(None));
static VALUES: Mutex<RefCell<BTreeMap<String, String>>> = Mutex::new(RefCell::new(BTreeMap::new()));

fn checksum(data: &[u8]) -> u32 {
    // FNV-1a: catches a torn or erased sector, which is all it is for.
    data.iter().fold(0x811c_9dc5u32, |h, &b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

/// Takes the flash and reads the stored values.
pub fn init(mut flash: Dev) {
    let mut best: Option<(usize, u32, BTreeMap<String, String>)> = None;
    let mut sector = alloc::vec![0u8; ERASE_SIZE];
    for (i, &off) in SECTORS.iter().enumerate() {
        if flash.blocking_read(off, &mut sector).is_err() {
            continue;
        }
        if let Some((generation, values)) = parse(&sector) {
            if best.as_ref().is_none_or(|b| generation > b.1) {
                best = Some((i, generation, values));
            }
        }
    }
    let (current, values) = match best {
        Some((i, g, v)) => (Some((i, g)), v),
        None => (None, BTreeMap::new()),
    };
    *STORE.0.borrow_mut() = Some(Store { flash, current });
    critical_section::with(|cs| *VALUES.borrow_ref_mut(cs) = values);
}

fn parse(sector: &[u8]) -> Option<(u32, BTreeMap<String, String>)> {
    if &sector[..4] != MAGIC {
        return None;
    }
    let generation = u32::from_le_bytes(sector[4..8].try_into().ok()?);
    let len = u32::from_le_bytes(sector[8..12].try_into().ok()?) as usize;
    if len > ERASE_SIZE - HEADER - 4 {
        return None;
    }
    let sum = u32::from_le_bytes(sector[HEADER + len..HEADER + len + 4].try_into().ok()?);
    if sum != checksum(&sector[4..HEADER + len]) {
        return None;
    }
    let text = core::str::from_utf8(&sector[HEADER..HEADER + len]).ok()?;
    let values = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Some((generation, values))
}

/// A copy of all stored values.
pub fn values() -> BTreeMap<String, String> {
    critical_section::with(|cs| VALUES.borrow_ref(cs).clone())
}

/// Changes the values with `f` and writes them to flash. Core 0 only.
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
    if body.len() > ERASE_SIZE - HEADER - 4 {
        return Err("configuration too large");
    }
    let mut store = STORE.0.borrow_mut();
    let store = store.as_mut().ok_or("flash not ready")?;
    let (next, generation) = match store.current {
        Some((i, g)) => (1 - i, g.wrapping_add(1)),
        None => (0, 1),
    };
    let mut sector: Vec<u8> = Vec::with_capacity(HEADER + body.len() + 4);
    sector.extend_from_slice(MAGIC);
    sector.extend_from_slice(&generation.to_le_bytes());
    sector.extend_from_slice(&(body.len() as u32).to_le_bytes());
    sector.extend_from_slice(body.as_bytes());
    let sum = checksum(&sector[4..]);
    sector.extend_from_slice(&sum.to_le_bytes());
    let off = SECTORS[next];
    store
        .flash
        .blocking_erase(off, off + ERASE_SIZE as u32)
        .map_err(|_| "flash erase failed")?;
    store
        .flash
        .blocking_write(off, &sector)
        .map_err(|_| "flash write failed")?;
    store.current = Some((next, generation));
    critical_section::with(|cs| *VALUES.borrow_ref_mut(cs) = values);
    Ok(())
}

/// Forgets everything (Wi-Fi, auth key, node identity). Core 0 only.
pub fn erase() -> Result<(), &'static str> {
    let mut store = STORE.0.borrow_mut();
    let store = store.as_mut().ok_or("flash not ready")?;
    critical_section::with(|cs| VALUES.borrow_ref_mut(cs).clear());
    store.current = None;
    for off in SECTORS {
        store
            .flash
            .blocking_erase(off, off + ERASE_SIZE as u32)
            .map_err(|_| "flash erase failed")?;
    }
    Ok(())
}
