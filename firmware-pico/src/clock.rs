//! The wall clock. The board has no battery-backed clock, so the time comes
//! from the `Date` header of an HTTP answer, and is never earlier than the
//! build. TLS checks certificates against it.

use embassy_time::Instant;
use portable_atomic::{AtomicU64, Ordering};
use rustls::pki_types::UnixTime;
use rustls::time_provider::TimeProvider;

const BUILD_UNIX_TIME: u64 = match u64::from_str_radix(env!("BUILD_UNIX_TIME"), 10) {
    Ok(t) => t,
    Err(_) => 0,
};

/// Unix seconds at boot (at `Instant` zero); 0 until set.
static BOOT_UNIX: AtomicU64 = AtomicU64::new(0);

/// The Unix time in seconds.
pub fn now() -> u64 {
    let up = Instant::now().as_secs();
    let boot = BOOT_UNIX.load(Ordering::Relaxed);
    (boot + up).max(BUILD_UNIX_TIME)
}

/// Sets the clock from an HTTP `Date` header; returns whether it parsed.
pub fn set_from_http_date(date: &str) -> bool {
    let Some(t) = parse_http_date(date) else {
        return false;
    };
    let up = Instant::now().as_secs();
    BOOT_UNIX.store(t.saturating_sub(up), Ordering::Relaxed);
    true
}

/// Parses the IMF-fixdate form, `Sun, 06 Nov 1994 08:49:37 GMT`, the only
/// one servers send today.
fn parse_http_date(s: &str) -> Option<u64> {
    let mut f = s.split_whitespace();
    let _weekday = f.next()?;
    let day: u64 = f.next()?.parse().ok()?;
    let month = match f.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = f.next()?.parse().ok()?;
    let mut hms = f.next()?.split(':').map(|x| x.parse::<u64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    if f.next()? != "GMT" || !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day as i64);
    u64::try_from(days)
        .ok()
        .map(|d| d * 86400 + h * 3600 + m * 60 + sec)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// [`now`] for rustls.
#[derive(Debug)]
pub struct Clock;

impl TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(core::time::Duration::from_secs(
            now(),
        )))
    }
}
