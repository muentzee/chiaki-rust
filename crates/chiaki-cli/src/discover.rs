//! `discover`-Subcommand: Discovery-Broadcast im LAN über den
//! `DiscoveryService` aus chiaki-core, tabellarische Ausgabe der Hosts.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chiaki_core::discovery::{discovery_host_state_string, DiscoveryHost};
use chiaki_core::discoveryservice::{DiscoveryService, DiscoveryServiceOptions};

use crate::util;

/// Argumente von `chiaki-cli discover`.
#[derive(Debug, Clone, clap::Args)]
pub struct DiscoverArgs {
    /// Millisekunden, die auf Antworten gewartet wird
    #[arg(long, default_value_t = 1000)]
    pub timeout_ms: u64,
    /// Zusätzliche (Subnetz-)Broadcast-Adressen, z. B. 192.168.1.255
    /// (mehrfach angebbar; Default: nur 255.255.255.255)
    #[arg(long = "broadcast-addr")]
    pub broadcast_addrs: Vec<String>,
}

/// Port von `chiaki_cli_cmd_discover` (cli/src/discover.c), als Broadcast
/// über den DiscoveryService statt Unicast an einen Host.
pub fn run(args: DiscoverArgs) -> Result<(), String> {
    // Limited Broadcast — der Service setzt den Port pro Ping selbst auf
    // 987 (PS4) bzw. 9302 (PS5).
    let send_addr: SocketAddr = "255.255.255.255:0".parse().expect("invariant");

    let mut broadcast_addrs = Vec::new();
    for s in &args.broadcast_addrs {
        let ip: IpAddr = s
            .parse()
            .map_err(|_| format!("invalid --broadcast-addr '{s}' (expected an IP)"))?;
        broadcast_addrs.push(SocketAddr::new(ip, 0));
    }

    let options = DiscoveryServiceOptions {
        hosts_max: 16,
        // C-Defaults (discovery.h): Host gilt nach 3 Pings ohne Antwort als weg.
        host_drop_pings: 3,
        ping_ms: 500,
        ping_initial_ms: 50,
        send_addr,
        broadcast_addrs,
        send_host: None,
    };

    // Live-Log: jeden neu entdeckten Host sofort melden.
    let printed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let cb_printed = Arc::clone(&printed);
    let cb: chiaki_core::discoveryservice::DiscoveryServiceCb = Arc::new(move |hosts| {
        let mut seen = cb_printed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for host in hosts {
            let id = host.host_id.clone().unwrap_or_default();
            if seen.insert(id) {
                tracing::info!(
                    "Discovered host: {} at {} ({}, {})",
                    host.host_name.as_deref().unwrap_or("(unnamed)"),
                    host.host_addr,
                    if host.is_ps5() { "PS5" } else { "PS4" },
                    discovery_host_state_string(host.state)
                );
            }
        }
    });

    let service = DiscoveryService::new(options, cb)
        .map_err(|e| format!("failed to start discovery service: {e}"))?;

    // Timeout abwarten (vorzeitiger Abbruch bei Ctrl+C).
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
    while Instant::now() < deadline && !util::ctrl_c_received() {
        std::thread::sleep(Duration::from_millis(50));
    }

    let hosts = service.hosts();
    service.fini();

    if hosts.is_empty() {
        println!("No hosts discovered.");
        return Ok(());
    }

    print!("{}", format_host_table(&hosts));
    Ok(())
}

/// Tabellarische Ausgabe aller gefundenen Hosts
/// (Name, Adresse, PS5/PS4, State, Running App).
pub fn format_host_table(hosts: &[DiscoveryHost]) -> String {
    const HEADERS: [&str; 5] = ["Name", "Address", "Type", "State", "Running App"];

    let rows: Vec<[String; 5]> = hosts
        .iter()
        .map(|h| {
            [
                h.host_name.clone().unwrap_or_else(|| "?".to_owned()),
                h.host_addr.clone(),
                if h.is_ps5() { "PS5" } else { "PS4" }.to_owned(),
                discovery_host_state_string(h.state).to_owned(),
                h.running_app_name
                    .clone()
                    .or_else(|| h.running_app_titleid.clone())
                    .unwrap_or_else(|| "-".to_owned()),
            ]
        })
        .collect();

    // Spaltenbreiten aus den Daten (mindestens so breit wie die Überschrift).
    let mut widths = HEADERS.map(|h| h.len());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }

    let line = |cells: [&String; 5]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };

    let header_cells: [&String; 5] =
        [&HEADERS[0].to_owned(), &HEADERS[1].to_owned(), &HEADERS[2].to_owned(), &HEADERS[3].to_owned(), &HEADERS[4].to_owned()];
    let mut out = String::new();
    out.push_str(&line(header_cells));
    out.push('\n');
    out.push_str(
        &"-".repeat(widths.iter().sum::<usize>() + 2 * (widths.len() - 1)),
    );
    out.push('\n');
    for row in &rows {
        let cells: [&String; 5] = [&row[0], &row[1], &row[2], &row[3], &row[4]];
        out.push_str(&line(cells));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::discovery::{DiscoveryHostState, DISCOVERY_PORT_PS4, DISCOVERY_PORT_PS5};

    fn host(name: &str, addr: &str, ps5: bool, state: DiscoveryHostState, app: &str) -> DiscoveryHost {
        DiscoveryHost {
            state,
            host_request_port: if ps5 { DISCOVERY_PORT_PS5 } else { DISCOVERY_PORT_PS4 },
            host_addr: addr.to_owned(),
            host_name: Some(name.to_owned()),
            host_id: Some(format!("id-{name}")),
            running_app_name: Some(app.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn table_layout() {
        let hosts = [
            host("PS5-1234", "192.168.0.42", true, DiscoveryHostState::Standby, "Fortnite"),
            host("ps4-livingroom", "192.168.0.7", false, DiscoveryHostState::Ready, "-"),
        ];
        let table = format_host_table(&hosts);
        let mut lines = table.lines();
        let header = lines.next().unwrap();
        assert!(header.starts_with("Name"));
        assert!(header.contains("Address"));
        assert!(header.contains("Running App"));

        let sep = lines.next().unwrap();
        assert!(sep.bytes().all(|b| b == b'-'));

        let row0 = lines.next().unwrap();
        // Zweite Zeile beginnt exakt in der "Address"-Spalte der ersten.
        assert!(row0.starts_with("PS5-1234"));
        assert!(row0.contains("192.168.0.42"));
        assert!(row0.contains("PS5"));
        assert!(row0.contains("standby"));
        assert!(row0.contains("Fortnite"));

        let row1 = lines.next().unwrap();
        assert!(row1.contains("ps4-livingroom"));
        assert!(row1.contains("PS4"));
        assert!(row1.contains("ready"));
        assert!(lines.next().is_none());

        // Spalten ausgerichtet: Index von "192.168.0.42" == Index von "192.168.0.7"
        let col0 = row0.find("192.168.0.42").unwrap();
        let col1 = row1.find("192.168.0.7").unwrap();
        assert_eq!(col0, col1);
    }
}
