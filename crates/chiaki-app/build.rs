//! Build-Script: bettet das App-Logo als EXE-/Fenster-Icon ein (Explorer +
//! Taskbar). Nur für Windows-Targets relevant.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../crates/chiaki-ui/assets/logo.ico");
    res.compile().expect("winresource: logo.ico einbetten");
}
