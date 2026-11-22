//! Randomness for tsnode (keys, nonces, transaction IDs).
//!
//! At boot 32 bytes come from the RP2350's true random number generator.
//! `getrandom` then draws from a ChaCha20 keystream under that key, and
//! replaces the key from the same keystream on every call, so earlier output
//! cannot be recovered from the state (fast key erasure). Reading the TRNG
//! on every call would be slow and would block with interrupts off.

use core::cell::RefCell;
use critical_section::Mutex;
use embassy_rp::peripherals::TRNG;
use embassy_rp::trng::Trng;

static KEY: Mutex<RefCell<Option<[u8; 32]>>> = Mutex::new(RefCell::new(None));

/// Seeds the generator from the TRNG; call before anything draws random
/// bytes.
pub async fn init(trng: &mut Trng<'static, TRNG>) {
    let t0 = embassy_time::Instant::now();
    let mut seed = [0u8; 32];
    trng.fill_bytes(&mut seed).await;
    let key = tsnode::crypto::blake2s(&[b"serial-over-tailnet rng", &seed]);
    critical_section::with(|cs| *KEY.borrow_ref_mut(cs) = Some(key));
    log::info!(
        "rng: seeded from the TRNG in {} ms",
        t0.elapsed().as_millis()
    );
}

fn fill(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    critical_section::with(|cs| {
        let mut key = KEY.borrow_ref_mut(cs);
        let k = key.as_mut().ok_or(getrandom::Error::UNSUPPORTED)?;
        // ChaCha20-Poly1305 of zeros is the ChaCha20 keystream (plus a tag).
        let zeros = alloc::vec![0u8; 32 + buf.len()];
        let ks = tsnode::crypto::aead_encrypt(k, 0, &zeros, &[]);
        k.copy_from_slice(&ks[..32]);
        buf.copy_from_slice(&ks[32..32 + buf.len()]);
        Ok(())
    })
}

getrandom::register_custom_getrandom!(fill);

/// getrandom 0.4's custom backend (the TLS provider uses that version).
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom04::Error> {
    // SAFETY: getrandom passes a buffer of `len` bytes, possibly
    // uninitialised; it is zeroed before it is used as a slice.
    let buf = unsafe {
        core::ptr::write_bytes(dest, 0, len);
        core::slice::from_raw_parts_mut(dest, len)
    };
    fill(buf).map_err(|_| getrandom04::Error::UNSUPPORTED)
}
