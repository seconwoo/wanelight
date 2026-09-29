//! Small helpers shared by the agent and the settings UI.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// `%APPDATA%\Wanelight` (or `WANELIGHT_DATA_DIR`), created on first use.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("WANELIGHT_DATA_DIR") {
        let dir = PathBuf::from(dir);
        let _ = fs::create_dir_all(&dir);
        return dir;
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("Wanelight");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Suffix that isolates instance-wide names (mutexes, window classes) when
/// `WANELIGHT_DATA_DIR` points somewhere else, so a test copy can run beside
/// the real one.
pub fn instance_suffix() -> String {
    match std::env::var_os("WANELIGHT_DATA_DIR") {
        Some(d) => format!(".{:08x}", fnv1a(d.to_string_lossy().to_ascii_lowercase().as_bytes()) as u32),
        None => String::new(),
    }
}

/// Verbose diagnostics, enabled with `WANELIGHT_DEBUG=1`.
pub fn debug_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WANELIGHT_DEBUG").is_some_and(|v| v != "0"))
}

/// Seconds since process start (monotonic).
pub fn now() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `bytes` to `path` atomically (temp file + rename) so readers never see a torn file.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// 64-bit FNV-1a, used for stable monitor ids.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub fn init_log(name: &str) {
    let path = data_dir().join(format!("{name}.log"));
    if fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = fs::rename(&path, path.with_extension("old.log"));
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
}

pub fn log_line(msg: &str) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, msg
    );
    if let Some(f) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::util::log_line(&format!($($t)*)) };
}

/// Lets a GUI-subsystem build print to the console it was started from.
pub fn attach_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Lower-case executable name of a process, e.g. "slack.exe".
pub fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase())
    }
}
