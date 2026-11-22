//! A panic resets the chip after leaving its message in RAM that survives
//! the reset, so the next boot can print it on the USB console (nothing
//! else would be seen: the console is gone by then).

use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

const MAGIC: u32 = 0x5041_4e43; // "PANC"
const SIZE: usize = 480;

#[repr(C)]
struct Record {
    magic: u32,
    len: u32,
    text: [u8; SIZE],
}

#[link_section = ".uninit.PANIC"]
static mut RECORD: MaybeUninit<Record> = MaybeUninit::uninit();

struct Writer<'a> {
    buf: &'a mut [u8; SIZE],
    len: usize,
}

impl Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(SIZE - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    cortex_m::interrupt::disable();
    // SAFETY: interrupts are off and the panic handler does not return; the
    // record is plain bytes, valid in any state.
    let rec = unsafe { &mut *core::ptr::addr_of_mut!(RECORD).cast::<Record>() };
    let mut w = Writer {
        buf: &mut rec.text,
        len: 0,
    };
    let _ = write!(w, "{info}");
    rec.len = w.len as u32;
    rec.magic = MAGIC;
    reset_chip()
}

/// Resets the whole chip but the oscillators, through the watchdog.
///
/// `SCB::sys_reset` is not enough: it resets the processors only, and the
/// USB controller stays as it was. The host then keeps the old device, which
/// no longer answers, and never enumerates the new one.
pub fn reset_chip() -> ! {
    use embassy_rp::pac;
    // Everything the power-on state machine can reset, but ROSC and XOSC
    // (bits 0 and 1), as embassy-rp's `Watchdog::trigger_reset` does.
    pac::PSM
        .wdsel()
        .write_value(pac::psm::regs::Wdsel(0x0001_ffff & !0b11));
    pac::WATCHDOG.ctrl().write(|w| w.set_trigger(true));
    loop {
        cortex_m::asm::nop();
    }
}

/// The message of a panic before the last reset, if there was one.
pub fn take() -> Option<alloc::string::String> {
    // SAFETY: called once at boot, before anything else touches the record.
    let rec = unsafe { &mut *core::ptr::addr_of_mut!(RECORD).cast::<Record>() };
    if rec.magic != MAGIC {
        return None;
    }
    rec.magic = 0;
    let len = (rec.len as usize).min(SIZE);
    Some(alloc::string::String::from_utf8_lossy(&rec.text[..len]).into_owned())
}
