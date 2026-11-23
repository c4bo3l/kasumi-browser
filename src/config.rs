#![allow(dead_code)]

use std::time::Duration;

/// Default landing page URL
pub const DEFAULT_HOME_URL: &str = "https://duckduckgo.com";

/// Default omnibox search query prefix
pub const DEFAULT_SEARCH_URL_PREFIX: &str = "https://duckduckgo.com/?q=";

/// Expanded width of the collapsible vertical workspace rail, in logical points.
pub const RAIL_WIDTH_EXPANDED: f32 = 180.0;

/// Width of the favicon-only strip left visible when the rail is collapsed.
pub const RAIL_WIDTH_COLLAPSED: f32 = 44.0;

/// Width of the floating Cmd/Ctrl+K command palette.
pub const COMMAND_PALETTE_WIDTH: f32 = 560.0;

/// Vertical offset of the command palette from the top of the window.
pub const COMMAND_PALETTE_TOP_OFFSET: f32 = 72.0;

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

/// Logical width of the native chrome rail for the current expansion state.
pub fn rail_width(expanded: bool) -> f32 {
    if expanded {
        RAIL_WIDTH_EXPANDED
    } else {
        RAIL_WIDTH_COLLAPSED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_widths_match_spec() {
        assert_eq!(RAIL_WIDTH_EXPANDED, 180.0);
        assert_eq!(RAIL_WIDTH_COLLAPSED, 44.0);
        assert_eq!(rail_width(true), RAIL_WIDTH_EXPANDED);
        assert_eq!(rail_width(false), RAIL_WIDTH_COLLAPSED);
        assert!(RAIL_WIDTH_COLLAPSED < RAIL_WIDTH_EXPANDED);
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

    #[test]
    fn command_palette_geometry_is_sane() {
        assert_eq!(COMMAND_PALETTE_WIDTH, 560.0);
        assert_eq!(COMMAND_PALETTE_TOP_OFFSET, 72.0);
        assert!(COMMAND_PALETTE_WIDTH < RAIL_WIDTH_EXPANDED * 4.0);
    }
}
