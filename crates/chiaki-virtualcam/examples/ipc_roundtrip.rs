// SPDX-License-Identifier: AGPL-3.0-only
//! Einmalige Verifikation der Headless-IPC aus Sicht der GUI:
//! 1. wartet bis eine Headless-Instanz läuft (is_running),
//! 2. liest die PID-Datei (Pfad als Argument — exe-relativ, muss zum
//!    Headless-Prozess passen),
//! 3. sendet request_stop,
//! 4. wartet bis die Instanz verschwunden ist.
use chiaki_virtualcam::{ipc, is_running, request_stop};

fn main() {
    let pid_path = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(ipc::default_pid_path);
    println!("warte auf Headless-Instanz ...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !is_running() {
        assert!(std::time::Instant::now() < deadline, "Instanz kam nicht hoch");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!("läuft: true, PID-Datei: {:?}", ipc::running_pid(&pid_path));
    assert!(ipc::running_pid(&pid_path).is_some(), "PID-Datei muss stehen");

    assert!(request_stop(), "request_stop muss den Event finden");
    println!("Stop-Signal gesendet, warte auf Ende ...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while is_running() {
        assert!(std::time::Instant::now() < deadline, "Instanz blieb hängen");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!(
        "Instanz beendet, PID-Datei entfernt: {:?}",
        ipc::running_pid(&pid_path).is_none()
    );
    println!("IPC-Roundtrip OK");
}
