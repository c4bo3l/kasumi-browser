#![allow(dead_code)]

use std::time::Duration;

/// Default landing page URL
pub const DEFAULT_HOME_URL: &str = "https://duckduckgo.com";

/// Default omnibox search query prefix
pub const DEFAULT_SEARCH_URL_PREFIX: &str = "https://duckduckgo.com/?q=";

/// Height of the top tab bar in points/logical pixels
pub const TAB_BAR_HEIGHT: f32 = 38.0;

/// Height of the omnibox/navigation toolbar in points/logical pixels
pub const NAV_BAR_HEIGHT: f32 = 38.0;

/// Total browser chrome height drawn natively via egui
pub const CHROME_HEIGHT: f32 = TAB_BAR_HEIGHT + NAV_BAR_HEIGHT; // 76.0 px

/// Default idle timeout before a background tab is eligible for hibernation
pub const TAB_IDLE_TIMEOUT_NORMAL: Duration = Duration::from_secs(600); // 10 minutes

/// Shrunk idle timeout when system memory is under high pressure (>= 80%)
pub const TAB_IDLE_TIMEOUT_PRESSURE: Duration = Duration::from_secs(30); // 30 seconds

/// RAM usage ratio (used / total) considered high pressure
pub const RAM_HIGH_PRESSURE_THRESHOLD: f32 = 0.80;

/// RAM usage ratio (used / total) considered critical pressure (triggers instant hibernation of all background tabs)
pub const RAM_CRITICAL_PRESSURE_THRESHOLD: f32 = 0.90;

/// How often system RAM is queried via sysinfo
pub const RAM_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// How often the tab manager watchdog tick runs
pub const TAB_WATCHDOG_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Upper bound on event-loop iterations spent draining CEF events during
/// shutdown. On macOS `on_before_close` is delivered by `cef::shutdown()`
/// itself, so waiting for it before shutdown would never complete.
pub const SHUTDOWN_MAX_WAIT_TICKS: u32 = 30;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_height_matches_spec() {
        assert_eq!(TAB_BAR_HEIGHT, 38.0);
        assert_eq!(NAV_BAR_HEIGHT, 38.0);
        assert_eq!(CHROME_HEIGHT, 76.0);
    }

    #[test]
    fn hibernation_timeouts_and_thresholds_are_ordered() {
        assert_eq!(TAB_IDLE_TIMEOUT_NORMAL, Duration::from_secs(600));
        assert_eq!(TAB_IDLE_TIMEOUT_PRESSURE, Duration::from_secs(30));
        assert!(TAB_IDLE_TIMEOUT_PRESSURE < TAB_IDLE_TIMEOUT_NORMAL);
        assert_eq!(RAM_HIGH_PRESSURE_THRESHOLD, 0.80);
        assert_eq!(RAM_CRITICAL_PRESSURE_THRESHOLD, 0.90);
        assert!(RAM_HIGH_PRESSURE_THRESHOLD < RAM_CRITICAL_PRESSURE_THRESHOLD);
        assert_eq!(RAM_CHECK_INTERVAL, Duration::from_secs(5));
        assert_eq!(TAB_WATCHDOG_TICK_INTERVAL, Duration::from_secs(1));
    }

    #[test]
    fn default_search_prefix_matches_home_origin() {
        assert!(DEFAULT_SEARCH_URL_PREFIX.starts_with(DEFAULT_HOME_URL));
        assert!(DEFAULT_SEARCH_URL_PREFIX.ends_with("?q="));
    }
}
