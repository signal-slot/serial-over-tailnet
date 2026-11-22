//! Randomness for tsnode (keys, nonces, transaction IDs).
//!
//! At boot 32 bytes come from the RP2350's true random number generator.
//! `getrandom` then draws from a ChaCha20 keystream under that key, and
//! replaces the key from the same keystream on every call, so earlier output
//! cannot be recovered from the state (fast key erasure). Reading the TRNG
//! on every call would be slow and would block with interrupts off.

use core::cell::RefCell;
use critical_section::Mutex;
use embassy_rp::pac;
use embassy_rp::peripherals::TRNG;
use embassy_rp::Peri;
use embassy_time::{Duration, Instant, Timer};

static KEY: Mutex<RefCell<Option<[u8; 32]>>> = Mutex::new(RefCell::new(None));

/// One TRNG block: 192 bits.
const BLOCK: usize = 24;
/// A block normally takes about 2 ms; one not ready by then is restarted.
const BLOCK_TIMEOUT: Duration = Duration::from_millis(20);

/// Seeds the generator from the TRNG; call before anything draws random
/// bytes.
///
/// The TRNG is read here rather than through embassy-rp's driver, which
/// waits for good when a block never finishes (the board stopped at boot,
/// before the shell started). A block that fails the autocorrelation test or
/// is not ready in time is started again after a soft reset.
pub async fn init(_trng: Peri<'static, TRNG>) {
    let t0 = Instant::now();
    let regs = pac::TRNG;
    let mut blocks = [[0u8; BLOCK]; 2];
    let (mut crngt, mut vn, mut autocorr, mut timeouts) = (0u32, 0u32, 0u32, 0u32);
    for block in blocks.iter_mut() {
        loop {
            start_block();
            let started = Instant::now();
            let ok = loop {
                if regs.trng_valid().read().ehr_valid() {
                    break true;
                }
                // A failed health test, or no block in time. (Busy is not
                // set yet right after the start, so it says nothing.)
                let isr = regs.rng_isr().read();
                if isr.crngt_err() || isr.vn_err() {
                    // The block starts over by itself.
                    if isr.crngt_err() {
                        crngt += 1;
                    } else {
                        vn += 1;
                    }
                    regs.rng_icr().write(|w| {
                        w.set_crngt_err(isr.crngt_err());
                        w.set_vn_err(isr.vn_err());
                    });
                }
                if isr.autocorr_err() {
                    // The TRNG stops until it is reset.
                    autocorr += 1;
                    break false;
                }
                if started.elapsed() > BLOCK_TIMEOUT {
                    timeouts += 1;
                    break false;
                }
                Timer::after_micros(100).await;
            };
            if ok {
                let words = [
                    regs.ehr_data0().read(),
                    regs.ehr_data1().read(),
                    regs.ehr_data2().read(),
                    regs.ehr_data3().read(),
                    regs.ehr_data4().read(),
                    // Read last: this clears the result registers.
                    regs.ehr_data5().read(),
                ];
                for (i, w) in words.iter().enumerate() {
                    block[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
                }
                break;
            }
        }
    }
    regs.rnd_source_enable().write(|w| w.set_rnd_src_en(false));
    let key = tsnode::crypto::blake2s(&[b"serial-over-tailnet rng", &blocks[0], &blocks[1]]);
    critical_section::with(|cs| *KEY.borrow_ref_mut(cs) = Some(key));
    log::info!(
        "rng: seeded from the TRNG in {} ms (crngt {crngt}, vn {vn}, autocorr {autocorr}, timeouts {timeouts})",
        t0.elapsed().as_millis()
    );
}

/// Resets the TRNG and starts one block, with the health tests on.
///
/// 400 samples per bit: with the 25 of the datasheet's example (embassy-rp's
/// default) the autocorrelation test failed thousands of times in a row on
/// our board, for up to 26 s; from 200 up no test failed with any chain
/// length, and a block takes about 2 ms.
fn start_block() {
    let regs = pac::TRNG;
    regs.rnd_source_enable().write(|w| w.set_rnd_src_en(false));
    regs.trng_sw_reset().write(|w| w.set_trng_sw_reset(true));
    // The datasheet asks for a delay after the soft reset; a read is enough.
    let _ = regs.trng_sw_reset().read();
    // Polled: no interrupts.
    regs.rng_imr().write(|w| {
        w.set_ehr_valid_int_mask(true);
        w.set_autocorr_err_int_mask(true);
        w.set_crngt_err_int_mask(true);
        w.set_vn_err_int_mask(true);
    });
    regs.rng_icr().write(|w| {
        w.set_ehr_valid(true);
        w.set_autocorr_err(true);
        w.set_crngt_err(true);
        w.set_vn_err(true);
    });
    regs.trng_config().write(|w| w.set_rnd_src_sel(1));
    regs.sample_cnt1().write_value(400);
    regs.trng_debug_control().write(|_| {});
    regs.rst_bits_counter()
        .write(|w| w.set_rst_bits_counter(true));
    regs.rnd_source_enable().write(|w| w.set_rnd_src_en(true));
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
