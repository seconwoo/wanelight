//! Display power and audio activity.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SC_MONITORPOWER, WM_SYSCOMMAND};
use windows::Win32::System::SystemInformation::GetTickCount;

/// Tick count of the last keyboard or mouse input (changes on any input).
pub fn last_input_tick() -> u32 {
    let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        let _ = GetLastInputInfo(&mut lii);
    }
    lii.dwTime
}

/// Seconds since the last keyboard or mouse input anywhere in the session.
pub fn idle_secs() -> f64 {
    let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if !GetLastInputInfo(&mut lii).as_bool() {
            return 0.0;
        }
        GetTickCount().wrapping_sub(lii.dwTime) as f64 / 1000.0
    }
}

/// Asks Windows to power displays down; any input wakes them again.
pub fn displays_off(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(SC_MONITORPOWER as usize), LPARAM(2));
    }
}

/// Peak meter on the default playback device.
pub struct AudioMeter {
    meter: Option<IAudioMeterInformation>,
    acquired_at: f64,
    retry_at: f64,
    last_sound: f64,
}

impl AudioMeter {
    pub fn new() -> Self {
        Self { meter: None, acquired_at: 0.0, retry_at: 0.0, last_sound: f64::NEG_INFINITY }
    }

    fn acquire() -> windows::core::Result<IAudioMeterInformation> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            dev.Activate(CLSCTX_ALL, None)
        }
    }

    pub fn poll(&mut self, now: f64) {
        // Re-acquire periodically so a changed default device is followed.
        if self.meter.is_some() && now - self.acquired_at > 60.0 {
            self.meter = None;
        }
        if self.meter.is_none() && now >= self.retry_at {
            match Self::acquire() {
                Ok(m) => {
                    self.meter = Some(m);
                    self.acquired_at = now;
                }
                Err(_) => self.retry_at = now + 30.0,
            }
        }
        if let Some(m) = &self.meter {
            match unsafe { m.GetPeakValue() } {
                Ok(p) if p > 0.001 => self.last_sound = now,
                Ok(_) => {}
                Err(_) => self.meter = None,
            }
        }
    }

    pub fn playing(&self, now: f64) -> bool {
        now - self.last_sound < 15.0
    }
}
