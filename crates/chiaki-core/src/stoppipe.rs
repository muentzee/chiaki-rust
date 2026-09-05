// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/stoppipe.c + lib/include/chiaki/stoppipe.h (chiaki-ng).
//
// Der C-Code verwendet unter Windows ein WSA-Event, unter POSIX eine Pipe und
// kombiniert beides in chiaki_stop_pipe_select_single() per select() mit
// Socket-FDs (inkl. chiaki_stop_pipe_connect() für nonblocking connects).
//
// Rust-Abbildung (std hat kein select() über Sockets):
// - Die StopPipe selbst ist ein Atomar-Stop-Flag (`Arc`-Teilung über `&StopPipe`
//   in ein Arc) mit Condvar für effizientes Warten.
// - chiaki_stop_pipe_stop()  -> StopPipe::stop()
// - chiaki_stop_pipe_reset() -> StopPipe::reset()
// - chiaki_stop_pipe_sleep() -> StopPipe::wait_timeout()   (Canceled | Timeout)
// - check()                  -> nonblocking-Variante: Err(Canceled) wenn gesetzt
// - is_set()                 -> reines Flag-Lesen für Poll-Schleifen
//
// select_single(stop_pipe, fd, read/write, timeout_ms)-Alternative für
// Empfangs-/Sendeschleifen (z. B. Takion-Recv-Loop):
//
//   // Socket BLOCKIEREND lassen; das Poll-Intervall ist das Read-Timeout
//   // (sock::recv_from_timeout setzt SO_RCVTIMEO). Zwischen zwei recv-Aufrufen
//   // wird das Stop-Flag geprüft — Reaktionszeit auf stop() <= POLL_INTERVAL.
//   loop {
//       stop_pipe.check()?;                       // Err(Canceled) wenn gestoppt
//       match sock::recv_from_timeout(&sock, &mut buf, POLL_INTERVAL) {
//           Ok((n, addr))  => { /* Paket verarbeiten */ }
//           Err(ChiakiError::Timeout) => continue,   // nichts empfangen -> weiter pollen
//           Err(e) => return Err(e),
//       }
//   }
//
// chiaki_stop_pipe_connect()-Alternative für TCP-connects (ctrl/regist):
//   TcpStream::connect_timeout() in Teilstücken (z. B. 500 ms) aufrufen und
//   dazwischen stop_pipe.check() prüfen — oder connect in einem Thread und
//   stop_pipe.wait_timeout() im Aufrufer.
// Siehe sock::send_to_timeout()/sock::recv_from_timeout().

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::error::{ChiakiError, ChiakiResult};

/// Port von `ChiakiStopPipe`.
pub struct StopPipe {
    stopped: Mutex<bool>,
    cond: Condvar,
}

impl Default for StopPipe {
    fn default() -> Self {
        Self::new()
    }
}

impl StopPipe {
    /// Port von `chiaki_stop_pipe_init()`.
    ///
    /// Kann — anders als das C-Original (WSACreateEvent/pipe) — nicht
    /// fehlschlagen, da kein OS-Handle angelegt wird; deshalb kein Result.
    pub fn new() -> Self {
        StopPipe {
            stopped: Mutex::new(false),
            cond: Condvar::new(),
        }
    }

    /// Port von `chiaki_stop_pipe_stop()`: setzt das Stop-Signal und weckt alle
    /// in `wait_timeout` blockierten Threads.
    pub fn stop(&self) {
        let mut stopped = self.lock();
        *stopped = true;
        self.cond.notify_all();
    }

    /// Setzt das Stop-Signal zurück (Port von `chiaki_stop_pipe_reset()`).
    ///
    /// Anders als im C-Original (das gepufferte Pipe-Bytes abliest) reicht
    /// hier das Zurücksetzen des Flags — es gibt nichts zu entleeren.
    pub fn reset(&self) {
        *self.lock() = false;
    }

    /// Ist das Stop-Signal gesetzt?
    pub fn is_set(&self) -> bool {
        *self.lock()
    }

    /// Nonblocking-Prüfung: `Err(ChiakiError::Canceled)` wenn gestoppt,
    /// sonst `Ok(())`. Für den Anfang jeder Poll-Iteration.
    pub fn check(&self) -> ChiakiResult<()> {
        if self.is_set() {
            Err(ChiakiError::Canceled)
        } else {
            Ok(())
        }
    }

    /// Port von `chiaki_stop_pipe_select_single()` ohne fd / `chiaki_stop_pipe_sleep()`.
    ///
    /// Wartet bis `timeout` abgelaufen ist oder `stop()` gerufen wird.
    ///
    /// - `ChiakiError::Canceled` → Stop-Signal gesetzt
    /// - `ChiakiError::Timeout`  → Zeit abgelaufen, nicht gestoppt
    ///
    /// C liefert dieselben Codes aus select_single; SUCCESS entfällt, da hier
    /// kein Socket-Event mitgewartet wird.
    pub fn wait_timeout(&self, timeout: Duration) -> ChiakiError {
        let deadline = Instant::now().checked_add(timeout);
        let mut stopped = self.lock();
        loop {
            if *stopped {
                return ChiakiError::Canceled;
            }
            let Some(remaining) = deadline.map(|d| d.saturating_duration_since(Instant::now())) else {
                return ChiakiError::Timeout;
            };
            if remaining.is_zero() {
                return if *stopped {
                    ChiakiError::Canceled
                } else {
                    ChiakiError::Timeout
                };
            }
            let (guard, _wait_result) = self
                .cond
                .wait_timeout(stopped, remaining)
                .unwrap_or_else(|e| e.into_inner());
            stopped = guard;
        }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        // Poisoned Mutex (Panic eines anderen Waiters) ist hier unkritisch:
        // das Flag selbst bleibt konsistent.
        self.stopped.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn new_is_not_stopped() {
        let sp = StopPipe::new();
        assert!(!sp.is_set());
        assert_eq!(sp.check(), Ok(()));
    }

    #[test]
    fn stop_sets_flag_and_check_canceled() {
        let sp = StopPipe::new();
        sp.stop();
        assert!(sp.is_set());
        assert_eq!(sp.check(), Err(ChiakiError::Canceled));
        // wait_timeout kehrt sofort mit Canceled zurück
        let t = Instant::now();
        assert_eq!(sp.wait_timeout(Duration::from_secs(10)), ChiakiError::Canceled);
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn reset_clears_flag() {
        let sp = StopPipe::new();
        sp.stop();
        assert!(sp.is_set());
        sp.reset();
        assert!(!sp.is_set());
        assert_eq!(sp.check(), Ok(()));
    }

    #[test]
    fn wait_timeout_times_out() {
        let sp = StopPipe::new();
        let t = Instant::now();
        assert_eq!(sp.wait_timeout(Duration::from_millis(20)), ChiakiError::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(15));
        assert!(!sp.is_set());
    }

    #[test]
    fn wait_timeout_wakes_on_stop_from_other_thread() {
        let sp = Arc::new(StopPipe::new());
        let sp2 = Arc::clone(&sp);
        let waiter = thread::spawn(move || {
            // würde 10 s blocken, wenn nicht gestoppt würde
            sp2.wait_timeout(Duration::from_secs(10))
        });
        thread::sleep(Duration::from_millis(50));
        sp.stop();
        assert_eq!(waiter.join().unwrap(), ChiakiError::Canceled);
    }

    #[test]
    fn multiple_waiters_wake_up() {
        let sp = Arc::new(StopPipe::new());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let sp = Arc::clone(&sp);
                thread::spawn(move || sp.wait_timeout(Duration::from_secs(10)))
            })
            .collect();
        thread::sleep(Duration::from_millis(50));
        sp.stop();
        for h in handles {
            assert_eq!(h.join().unwrap(), ChiakiError::Canceled);
        }
    }
}
