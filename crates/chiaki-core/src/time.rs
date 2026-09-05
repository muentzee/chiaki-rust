// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/time.c + lib/include/chiaki/time.h (chiaki-ng).

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> &'static Instant {
    EPOCH.get_or_init(Instant::now)
}

/// Port von `chiaki_time_now_monotonic_us()` (lib/src/time.c).
///
/// C nutzt unter Windows QueryPerformanceCounter (µs seit Systemstart).
/// `std::time::Instant` ist ebenfalls monotonic (QPC-basiert), hat aber eine
/// willkürliche Epoche — hier der Zeitpunkt des ersten Aufrufs. Relative
/// Differenzen sind identisch zum C-Verhalten; absolute Werte sind nicht mit
/// C-Werten vergleichbar (müssen sie auch nie sein, chiaki rechnet nur mit
/// Differenzen).
pub fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// Port von `chiaki_time_now_monotonic_ms()` (lib/include/chiaki/time.h).
pub fn now_ms() -> u64 {
    now_us() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn now_us_is_monotonic() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
        assert!(a > 0 || b > 0);
    }

    #[test]
    fn now_us_increases_over_sleep() {
        let a = now_us();
        sleep(Duration::from_millis(5));
        let b = now_us();
        assert!(b > a, "monotone µs-Zeit muss über 5 ms sleep wachsen: {a} -> {b}");
    }

    #[test]
    fn now_ms_matches_now_us() {
        let ms = now_ms();
        let us = now_us();
        // ms ist ein abgeleiteter Wert aus derselben Quelle
        assert!(us / 1000 >= ms.saturating_sub(1));
    }
}
