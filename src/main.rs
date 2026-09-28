#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod autostart;
mod config;
mod hardening;
mod icon_art;
mod ipc;
mod ledger;
mod ui;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let exclude_from_capture = !has("--no-capture-exclusion");
    let code = match args.first().map(String::as_str) {
        Some("--ui") => ui::run(args.get(1).map(String::as_str).unwrap_or("overview")),
        Some("--selftest") => agent::selftest::run(exclude_from_capture, has("--map")),
        Some("--test-surface") => agent::selftest::surface(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60)),
        Some("--status") => {
            util::attach_console();
            if ipc::send_command(ipc::CMD_WRITE_STATUS, 0) {
                std::thread::sleep(std::time::Duration::from_millis(400));
                println!("{}", std::fs::read_to_string(ipc::status_path()).unwrap_or_default());
                0
            } else {
                println!("Wanelight is not running");
                1
            }
        }
        Some("--quit") => {
            if ipc::send_command(ipc::CMD_QUIT, 0) { 0 } else { 1 }
        }
        _ => agent::run(agent::Options { exclude_from_capture }),
    };
    std::process::exit(code);
}
