//! Quartz Systems Lumen appliance installer.
//!
//! Normally runs full-screen under gnome-kiosk in the live installer
//! environment. `--print-plan <config.json>` prints the install plan for a
//! given configuration and exits without starting GTK (dry-run/debugging).

mod config;
mod engine;
mod sysinfo;
mod ui;

use gtk::glib;
use gtk::prelude::*;
use std::io::Write;

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--print-plan") {
        return print_plan(args.get(2).map(String::as_str));
    }

    // Smoke-test marker: proves kernel -> dracut -> squashfs -> logind ->
    // kiosk -> GTK app came up. CI greps the serial console for this.
    emit_ready_marker();

    let app = gtk::Application::builder()
        .application_id("net.quartzsystems.lumen.installer")
        .build();
    app.connect_startup(|_| ui::load_css());
    app.connect_activate(ui::build);
    // Empty args: GTK must not try to parse installer flags.
    app.run_with_args::<&str>(&[])
}

fn print_plan(config_path: Option<&str>) -> glib::ExitCode {
    let Some(path) = config_path else {
        eprintln!("usage: lumen-installer --print-plan <config.json>");
        return glib::ExitCode::FAILURE;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("read {path}: {err}");
            return glib::ExitCode::FAILURE;
        }
    };
    let cfg: config::InstallConfig = match serde_json::from_str(&text) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("parse {path}: {err}");
            return glib::ExitCode::FAILURE;
        }
    };
    engine::print_plan(&engine::plan::build_plan(&cfg, &config::BuildPins::load()));
    glib::ExitCode::SUCCESS
}

fn emit_ready_marker() {
    println!("LUMEN-INSTALLER-READY");
    if let Ok(mut serial) = std::fs::OpenOptions::new().write(true).open("/dev/ttyS0") {
        let _ = writeln!(serial, "LUMEN-INSTALLER-READY");
    }
}
