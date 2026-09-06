// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//! Schmales Start-Binary der chiaki-ui (für `cargo run -p chiaki-ui`).
//!
//! chiaki-app braucht später nur noch `chiaki_ui::run()` aufzurufen —
//! dieses Binary ist reiner Dev-Einstieg und enthält keine Logik.
//! (Kein Profil — `--profile` bietet nur das chiaki-Binary an.)

fn main() -> gpui::Result<()> {
    chiaki_ui::run(None)
}
