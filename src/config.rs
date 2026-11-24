#![allow(dead_code)]

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default landing page URL
pub const DEFAULT_HOME_URL: &str = "https://duckduckgo.com";

/// Default omnibox search query prefix
pub const DEFAULT_SEARCH_URL_PREFIX: &str = "https://duckduckgo.com/?q=";

/// Expanded width of the collapsible vertical workspace rail, in logical points.
pub const RAIL_WIDTH_EXPANDED: f32 = 208.0;

/// Width of the favicon-only strip left visible when the rail is collapsed.
pub const RAIL_WIDTH_COLLAPSED: f32 = 44.0;

/// Width of the floating Cmd/Ctrl+K command palette.
pub const COMMAND_PALETTE_WIDTH: f32 = 560.0;

/// Vertical offset of the command palette from the top of the window.
pub const COMMAND_PALETTE_TOP_OFFSET: f32 = 72.0;

/// SQLite key under which the full preferences JSON document is stored.
pub const PREFERENCES_DB_KEY: &str = "browser_preferences";

/// Range accepted by the hibernation timeout setting.
pub const MIN_IDLE_TIMEOUT_MINUTES: u32 = 1;
pub const MAX_IDLE_TIMEOUT_MINUTES: u32 = 60;

/// Whether the browser follows the OS appearance or forces one appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

impl Default for ThemePreference {
    fn default() -> Self {
        Self::System
    }
}

/// Search provider used for omnibox text that is not a URL or domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    DuckDuckGo,
    Google,
    Bing,
}

impl SearchEngine {
    pub const ALL: [Self; 3] = [Self::DuckDuckGo, Self::Google, Self::Bing];

    pub fn label(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "DuckDuckGo",
            Self::Google => "Google",
            Self::Bing => "Bing",
        }
    }

    pub fn search_url_prefix(self) -> &'static str {
        match self {
            Self::DuckDuckGo => DEFAULT_SEARCH_URL_PREFIX,
            Self::Google => "https://www.google.com/search?q=",
            Self::Bing => "https://www.bing.com/search?q=",
        }
    }

    /// Landing page used by a fresh tab. It follows the selected provider so
    /// changing the default search engine changes both search and new tabs.
    pub fn home_url(self) -> &'static str {
        match self {
            Self::DuckDuckGo => DEFAULT_HOME_URL,
            Self::Google => "https://www.google.com",
            Self::Bing => "https://www.bing.com",
        }
    }
}

impl Default for SearchEngine {
    fn default() -> Self {
        Self::DuckDuckGo
    }
}

/// Material Symbols icon chosen by the user for a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIcon {
    Work,
    Code,
    School,
    Rocket,
    Palette,
    Music,
    Book,
    Public,
}

impl WorkspaceIcon {
    pub const ALL: [Self; 8] = [
        Self::Work,
        Self::Code,
        Self::School,
        Self::Rocket,
        Self::Palette,
        Self::Music,
        Self::Book,
        Self::Public,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::Code => "Code",
            Self::School => "Study",
            Self::Rocket => "Launch",
            Self::Palette => "Design",
            Self::Music => "Music",
            Self::Book => "Reading",
            Self::Public => "Web",
        }
    }

    pub fn storage_key(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Code => "code",
            Self::School => "school",
            Self::Rocket => "rocket",
            Self::Palette => "palette",
            Self::Music => "music",
            Self::Book => "book",
            Self::Public => "public",
        }
    }

    pub fn from_storage_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|icon| icon.storage_key() == key)
            .unwrap_or_default()
    }
}

impl Default for WorkspaceIcon {
    fn default() -> Self {
        Self::Work
    }
}

/// Accent colour chosen by the user for a workspace. It tints the native
/// chrome only; page content keeps Chromium's own colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceColor {
    Blue,
    Purple,
    Green,
    Orange,
    Red,
    Teal,
    Pink,
    Slate,
}

impl WorkspaceColor {
    pub const ALL: [Self; 8] = [
        Self::Blue,
        Self::Purple,
        Self::Green,
        Self::Orange,
        Self::Red,
        Self::Teal,
        Self::Pink,
        Self::Slate,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Blue => "Blue",
            Self::Purple => "Purple",
            Self::Green => "Green",
            Self::Orange => "Orange",
            Self::Red => "Red",
            Self::Teal => "Teal",
            Self::Pink => "Pink",
            Self::Slate => "Slate",
        }
    }

    pub fn storage_key(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Green => "green",
            Self::Orange => "orange",
            Self::Red => "red",
            Self::Teal => "teal",
            Self::Pink => "pink",
            Self::Slate => "slate",
        }
    }

    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Blue => (96, 165, 250),
            Self::Purple => (167, 139, 250),
            Self::Green => (74, 222, 128),
            Self::Orange => (251, 146, 60),
            Self::Red => (248, 113, 113),
            Self::Teal => (45, 212, 191),
            Self::Pink => (244, 114, 182),
            Self::Slate => (148, 163, 184),
        }
    }

    pub fn from_storage_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|color| color.storage_key() == key)
            .unwrap_or_default()
    }
}

impl Default for WorkspaceColor {
    fn default() -> Self {
        Self::Blue
    }
}

/// A native workspace. Workspaces own their tab lists, pinned tabs, and a
/// bookmark folder named after the workspace. Bookmarks stay globally visible
/// so a page saved in one workspace can be opened from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: u64,
    pub name: String,
    pub icon: WorkspaceIcon,
    pub color: WorkspaceColor,
    /// Fallback workspace when no last-active workspace is available, and the
    /// choice offered to new windows. `is_active` follows the workspace open at
    /// shutdown and wins on startup.
    pub is_default: bool,
    pub is_active: bool,
}

impl Workspace {
    pub const DEFAULT_ID: u64 = 1;
    pub const DEFAULT_NAME: &'static str = "Personal";
    pub const MAX_NAME_LEN: usize = 32;

    pub fn default_workspace() -> Self {
        Self {
            id: Self::DEFAULT_ID,
            name: Self::DEFAULT_NAME.to_string(),
            icon: WorkspaceIcon::Work,
            color: WorkspaceColor::Blue,
            is_default: true,
            is_active: true,
        }
    }

    /// Trim and clamp names coming from the UI or persisted storage.
    pub fn normalize_name(name: &str) -> String {
        let trimmed = name.trim();
        let mut normalized = String::with_capacity(trimmed.len().min(Self::MAX_NAME_LEN));
        for (index, ch) in trimmed.chars().enumerate() {
            if index >= Self::MAX_NAME_LEN {
                break;
            }
            normalized.push(ch);
        }
        if normalized.is_empty() {
            Self::DEFAULT_NAME.to_string()
        } else {
            normalized
        }
    }

    pub fn archived_folder(name: &str) -> String {
        format!("Archived {name}")
    }
}

/// Which workspace Kasumi opens when the browser launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStartupBehavior {
    /// Reopen the space that was active when the last session closed.
    LastActive,
    /// Always open the space marked as default in Settings.
    DefaultWorkspace,
}

impl Default for WorkspaceStartupBehavior {
    fn default() -> Self {
        Self::LastActive
    }
}

impl WorkspaceStartupBehavior {
    pub const ALL: [Self; 2] = [Self::LastActive, Self::DefaultWorkspace];

    pub fn label(self) -> &'static str {
        match self {
            Self::LastActive => "Last used space",
            Self::DefaultWorkspace => "Default space",
        }
    }
}

/// User-configurable browser preferences. The whole document is persisted as
/// JSON by the background SQLite worker so adding a field never requires a
/// schema migration or writes from a CEF callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserPreferences {
    pub theme: ThemePreference,
    pub rail_expanded: bool,
    pub adblock_enabled: bool,
    pub idle_timeout_minutes: u32,
    pub search_engine: SearchEngine,
    /// Workspace selected when the browser launches.
    pub workspace_startup: WorkspaceStartupBehavior,
    /// Custom download directory. Empty means the OS default Downloads folder.
    pub download_directory: String,
    /// Show the native save dialog for each download instead of auto-saving.
    pub ask_where_to_save: bool,
}

impl Default for BrowserPreferences {
    fn default() -> Self {
        Self {
            theme: ThemePreference::default(),
            rail_expanded: true,
            adblock_enabled: true,
            idle_timeout_minutes: 10,
            search_engine: SearchEngine::default(),
            workspace_startup: WorkspaceStartupBehavior::default(),
            download_directory: String::new(),
            ask_where_to_save: false,
        }
    }
}

impl BrowserPreferences {
    /// Clamp values that could have been edited outside the Settings UI.
    pub fn normalized(self) -> Self {
        Self {
            idle_timeout_minutes: normalize_idle_timeout_minutes(self.idle_timeout_minutes),
            ..self
        }
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_minutes as u64 * 60)
    }

    pub fn search_url_prefix(&self) -> &'static str {
        self.search_engine.search_url_prefix()
    }

    pub fn home_url(&self) -> &'static str {
        self.search_engine.home_url()
    }

    /// Resolve the folder downloads should be written into. The explicit
    /// preference wins; otherwise use the platform's standard Downloads
    /// location, falling back to a `Downloads` folder under the home directory.
    pub fn download_directory(&self) -> std::path::PathBuf {
        if !self.download_directory.trim().is_empty() {
            return std::path::PathBuf::from(&self.download_directory);
        }
        default_download_directory()
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json).map(Self::normalized)
    }
}

/// Platform default download folder, used when no custom directory is set.
/// Honors `XDG_DOWNLOAD_DIR` on Linux and falls back to `$HOME/Downloads`.
pub fn default_download_directory() -> std::path::PathBuf {
    if let Some(downloads) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        if !downloads.is_empty() {
            return std::path::PathBuf::from(downloads);
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Downloads")
}

pub fn normalize_idle_timeout_minutes(minutes: u32) -> u32 {
    minutes.clamp(MIN_IDLE_TIMEOUT_MINUTES, MAX_IDLE_TIMEOUT_MINUTES)
}

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

/// Simultaneous HTTP Range requests used by the native download manager.
pub const PARALLEL_DOWNLOAD_SEGMENTS: u8 = 8;

/// Files below this size are downloaded with one request; segment overhead
/// costs more than it saves for small payloads.
pub const PARALLEL_DOWNLOAD_MIN_BYTES: i64 = 1024 * 1024;

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
        assert_eq!(RAIL_WIDTH_EXPANDED, 208.0);
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
    fn parallel_download_limits_are_sane() {
        assert_eq!(PARALLEL_DOWNLOAD_SEGMENTS, 8);
        assert_eq!(PARALLEL_DOWNLOAD_MIN_BYTES, 1024 * 1024);
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

    #[test]
    fn default_preferences_match_shipped_behavior() {
        let prefs = BrowserPreferences::default();
        assert_eq!(prefs.theme, ThemePreference::System);
        assert!(prefs.rail_expanded);
        assert!(prefs.adblock_enabled);
        assert_eq!(prefs.idle_timeout(), TAB_IDLE_TIMEOUT_NORMAL);
        assert_eq!(prefs.search_engine, SearchEngine::DuckDuckGo);
        assert_eq!(prefs.search_url_prefix(), DEFAULT_SEARCH_URL_PREFIX);
        assert_eq!(
            prefs.workspace_startup,
            WorkspaceStartupBehavior::LastActive
        );
        assert!(prefs.download_directory.is_empty());
        assert!(!prefs.ask_where_to_save);
        assert!(prefs.download_directory().ends_with("Downloads"));
    }

    #[test]
    fn preferences_json_round_trip_and_normalize_values() {
        let prefs = BrowserPreferences {
            theme: ThemePreference::Light,
            rail_expanded: false,
            adblock_enabled: false,
            idle_timeout_minutes: 500,
            search_engine: SearchEngine::Bing,
            workspace_startup: WorkspaceStartupBehavior::DefaultWorkspace,
            download_directory: "/tmp/kasumi-downloads".to_string(),
            ask_where_to_save: true,
        };
        let json = prefs.to_json().unwrap_or_default();
        assert!(json.contains("\"workspace_startup\":\"default_workspace\""));
        let loaded = BrowserPreferences::from_json(&json).unwrap_or_default();
        assert_eq!(loaded.theme, ThemePreference::Light);
        assert!(!loaded.rail_expanded);
        assert!(!loaded.adblock_enabled);
        assert_eq!(loaded.idle_timeout_minutes, MAX_IDLE_TIMEOUT_MINUTES);
        assert_eq!(loaded.search_engine, SearchEngine::Bing);
        assert_eq!(loaded.idle_timeout(), Duration::from_secs(3600));
        assert_eq!(
            loaded.workspace_startup,
            WorkspaceStartupBehavior::DefaultWorkspace
        );
        assert_eq!(loaded.download_directory, "/tmp/kasumi-downloads");
        assert!(loaded.ask_where_to_save);
        assert_eq!(
            loaded.download_directory(),
            std::path::PathBuf::from("/tmp/kasumi-downloads")
        );

        assert_eq!(normalize_idle_timeout_minutes(0), MIN_IDLE_TIMEOUT_MINUTES);
        assert_eq!(normalize_idle_timeout_minutes(12), 12);
        assert_eq!(
            normalize_idle_timeout_minutes(u32::MAX),
            MAX_IDLE_TIMEOUT_MINUTES
        );
        assert!(BrowserPreferences::from_json("not json").is_err());
    }

    #[test]
    fn search_engines_have_distinct_query_prefixes_and_homepages() {
        let prefixes: Vec<_> = SearchEngine::ALL
            .iter()
            .map(|engine| engine.search_url_prefix())
            .collect();
        let homepages: Vec<_> = SearchEngine::ALL
            .iter()
            .map(|engine| engine.home_url())
            .collect();
        assert_eq!(prefixes.len(), 3);
        assert!(prefixes.iter().all(|prefix| prefix.ends_with("?q=")));
        for (index, prefix) in prefixes.iter().enumerate() {
            assert!(!prefixes[index + 1..].contains(prefix));
        }
        assert_eq!(homepages.len(), 3);
        for (index, homepage) in homepages.iter().enumerate() {
            assert!(!homepages[index + 1..].contains(homepage));
            assert!(prefixes[index].starts_with(homepage));
        }
        assert_eq!(
            BrowserPreferences {
                search_engine: SearchEngine::Google,
                ..BrowserPreferences::default()
            }
            .home_url(),
            "https://www.google.com"
        );
    }
}
