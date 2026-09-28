//! Agent <-> settings window communication.
//!
//! The settings window posts a registered window message to the agent's hidden
//! window; the agent answers by writing small files into the data directory.

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, RegisterWindowMessageW};
use windows::core::PCWSTR;

use crate::util;

const AGENT_CLASS_BASE: &str = "WanelightAgentWindow";

pub fn agent_class() -> String {
    format!("{AGENT_CLASS_BASE}{}", util::instance_suffix())
}
pub const COMMAND_MESSAGE: &str = "Wanelight.Command.v1";

pub const CMD_WRITE_STATUS: usize = 1;
pub const CMD_FLUSH_LEDGER: usize = 2;
/// lparam = minutes; 0 = until resumed.
pub const CMD_PAUSE: usize = 3;
pub const CMD_RESUME: usize = 4;
pub const CMD_RELOAD_CONFIG: usize = 5;
pub const CMD_QUIT: usize = 6;
pub const CMD_RESET_LEDGER: usize = 7;

pub fn command_message_id() -> u32 {
    let name = util::wide(COMMAND_MESSAGE);
    unsafe { RegisterWindowMessageW(PCWSTR(name.as_ptr())) }
}

/// Sends a command to a running agent. Returns false if no agent is running.
pub fn send_command(cmd: usize, arg: isize) -> bool {
    let class = util::wide(&agent_class());
    unsafe {
        match FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                PostMessageW(Some(hwnd), command_message_id(), WPARAM(cmd), LPARAM(arg)).is_ok()
            }
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Status {
    pub written_at: u64,
    pub version: String,
    pub enabled: bool,
    pub paused: bool,
    /// Seconds until an automatic resume; None when paused indefinitely.
    pub pause_remaining_secs: Option<u64>,
    /// "active", "away", "display-off", "paused", "excluded-app".
    pub state: String,
    pub idle_secs: u64,
    pub panel_hours_since_rest: f32,
    pub monitors: Vec<MonitorStatus>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MonitorStatus {
    pub id: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub hdr: bool,
    pub capturing: bool,
    pub enabled: bool,
    /// Fraction of the screen currently dimmed by more than 2 %.
    pub dimmed_fraction: f32,
    /// Strongest dim currently applied anywhere on this monitor.
    pub max_dim: f32,
    /// Fraction of the screen that has been static for over a minute.
    pub static_fraction: f32,
    pub ddc_supported: Option<bool>,
}

pub fn status_path() -> std::path::PathBuf {
    util::data_dir().join("status.json")
}

pub fn read_status() -> Option<Status> {
    let text = std::fs::read_to_string(status_path()).ok()?;
    serde_json::from_str(&text).ok()
}
