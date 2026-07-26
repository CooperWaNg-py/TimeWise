//! Idle detection: pause tracking when the user hasn't touched input for a
//! while (iteration 2, user-confirmed 5-minute default). Fixes "tracking
//! continues while the screen is locked".
//!
//! Tradeoff (confirmed with the user): activities with no input — e.g.
//! watching a long movie — do not accrue time past the threshold.

use crate::tracker::ActiveWindow;

pub trait IdleSource: Send {
    /// Seconds since the last keyboard/mouse input. 0.0 on failure (fail-open:
    /// unknown idle means "active", we never pause on uncertainty).
    fn idle_seconds(&mut self) -> f64;
}

// ---- macOS: CGEventSourceSecondsSinceLastEventType ----

#[cfg(target_os = "macos")]
pub struct SystemIdle;

#[cfg(target_os = "macos")]
mod ffi {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    }
}

#[cfg(target_os = "macos")]
impl IdleSource for SystemIdle {
    fn idle_seconds(&mut self) -> f64 {
        // kCGEventSourceStateCombinedSessionState = 0, kCGAnyInputEventType = ~0
        unsafe { ffi::CGEventSourceSecondsSinceLastEventType(0, u32::MAX) }
    }
}

// ---- Windows: GetLastInputInfo ----

#[cfg(target_os = "windows")]
pub struct SystemIdle;

#[cfg(target_os = "windows")]
impl IdleSource for SystemIdle {
    fn idle_seconds(&mut self) -> f64 {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        let ok = unsafe { GetLastInputInfo(&mut lii) };
        if !ok.as_bool() {
            return 0.0;
        }
        let elapsed = unsafe { GetTickCount() }.wrapping_sub(lii.dwTime);
        elapsed as f64 / 1000.0
    }
}

// ---- Other platforms: unknown, always active ----

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub struct SystemIdle;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl IdleSource for SystemIdle {
    fn idle_seconds(&mut self) -> f64 {
        0.0
    }
}

/// App names that must never be recorded (the tracker must not record itself).
pub const SELF_APP_NAMES: [&str; 3] = ["timewise-app", "TimeWise", "timewise"];

pub fn is_self_app(app_name: &str) -> bool {
    SELF_APP_NAMES.iter().any(|s| app_name.eq_ignore_ascii_case(s))
}

/// Apply the tracking gates to a raw observation:
/// - idle past threshold -> pause (None)
/// - TimeWise's own window -> pause (None); the app never records itself
pub fn gated_window(
    window: Option<ActiveWindow>,
    idle_s: f64,
    idle_threshold_s: i64,
) -> Option<ActiveWindow> {
    if idle_s > idle_threshold_s as f64 {
        return None;
    }
    match window {
        Some(w) if is_self_app(&w.app_name) => None,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(app: &str) -> Option<ActiveWindow> {
        Some(ActiveWindow { app_name: app.into(), title: "t".into() })
    }

    #[test]
    fn idle_pauses_tracking() {
        assert!(gated_window(win("Roblox"), 299.0, 300).is_some());
        assert!(gated_window(win("Roblox"), 301.0, 300).is_none());
        assert!(gated_window(None, 301.0, 300).is_none());
    }

    #[test]
    fn self_app_is_never_recorded() {
        assert!(gated_window(win("timewise-app"), 0.0, 300).is_none());
        assert!(gated_window(win("TimeWise"), 0.0, 300).is_none());
        assert!(gated_window(win("TIMEWISE-APP"), 0.0, 300).is_none());
        assert!(gated_window(win("Roblox"), 0.0, 300).is_some());
    }
}
