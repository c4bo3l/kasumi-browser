//! egui + wgpu browser chrome: a collapsible vertical workspace rail plus a
//! floating Cmd/Ctrl+K command palette.
//!
//! # Architecture
//! `ChromeRenderer` owns the wgpu surface, device/queue, and egui context.
//! Each frame the caller:
//! 1. Updates `ChromeState` with the current tab list / active-tab info.
//! 2. Calls `render()` → draws the chrome and returns any `UiAction`s the user triggered.
//!
//! The CEF content renders in a separate native OS child view that starts at
//! `x = rail_width` and fills the remaining window height. The command palette,
//! downloads bubble, and rail tooltips use transparent native overlay windows so
//! they appear above that child view; the browser keeps running underneath and
//! the page stays visible. This file never touches CEF directly.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use egui::{
    pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Frame, Layout, Order, Response,
    RichText, Rounding, Sense, Stroke, TextEdit, TextureOptions, Ui, UiBuilder,
};
use egui_wgpu::ScreenDescriptor;

use crate::config::{
    rail_width, BrowserPreferences, SearchEngine, ThemePreference, Workspace, WorkspaceColor,
    WorkspaceIcon, WorkspaceStartupBehavior, COMMAND_PALETTE_TOP_OFFSET, COMMAND_PALETTE_WIDTH,
    DEFAULT_SEARCH_URL_PREFIX, MAX_IDLE_TIMEOUT_MINUTES, MIN_IDLE_TIMEOUT_MINUTES,
};
use crate::events::{DownloadEngine, DownloadInfo, DownloadState, UiAction};

/// Light or dark chrome appearance. The OS theme decides the default, and
/// `ChromeTheme::from_winit` maps live `ThemeChanged` events into this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromeTheme {
    #[default]
    Dark,
    Light,
}

/// Colours for every native chrome surface. Keeping the two themes in one
/// struct makes it impossible for a new control to silently stay dark when the
/// OS switches to light mode.
#[derive(Debug, Clone, Copy)]
pub struct ChromePalette {
    pub rail_bg: Color32,
    pub rail_surface: Color32,
    pub rail_row_active: Color32,
    pub rail_row_hover: Color32,
    pub rail_accent: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_strong: Color32,
    pub brand: Color32,
    pub section_label: Color32,
    pub shield_ok: Color32,
    pub scrim: Color32,
    pub palette_bg: Color32,
    pub palette_border: Color32,
    pub palette_subtext: Color32,
    pub palette_hint: Color32,
    pub palette_row_selected: Color32,
    pub palette_row_hover: Color32,
    pub menu_bg: Color32,
    pub menu_border: Color32,
    pub menu_disabled: Color32,
    pub close_bg: Color32,
    pub close_icon: Color32,
    pub tab_border: Color32,
    pub loading: Color32,
    pub tooltip_bg: Color32,
    pub tooltip_border: Color32,
    pub tooltip_text: Color32,
}

impl ChromePalette {
    pub const DARK: Self = Self {
        rail_bg: Color32::from_rgb(24, 25, 29),
        rail_surface: Color32::from_rgb(38, 44, 58),
        rail_row_active: Color32::from_rgb(52, 75, 112),
        rail_row_hover: Color32::from_rgb(42, 46, 54),
        rail_accent: Color32::from_rgb(110, 170, 255),
        text: Color32::from_rgb(214, 218, 226),
        text_dim: Color32::from_rgb(140, 148, 162),
        text_strong: Color32::WHITE,
        brand: Color32::from_rgb(150, 185, 235),
        section_label: Color32::from_rgb(125, 130, 140),
        shield_ok: Color32::from_rgb(80, 200, 120),
        scrim: Color32::from_rgba_premultiplied(0, 0, 0, 150),
        palette_bg: Color32::from_rgb(30, 32, 38),
        palette_border: Color32::from_rgb(72, 88, 116),
        palette_subtext: Color32::from_rgb(155, 160, 170),
        palette_hint: Color32::from_rgb(120, 125, 135),
        palette_row_selected: Color32::from_rgb(58, 82, 120),
        palette_row_hover: Color32::from_rgb(44, 48, 56),
        menu_bg: Color32::from_rgb(26, 29, 36),
        menu_border: Color32::from_rgb(90, 110, 142),
        menu_disabled: Color32::from_rgb(105, 111, 122),
        close_bg: Color32::from_rgb(64, 70, 84),
        close_icon: Color32::WHITE,
        tab_border: Color32::from_rgb(86, 128, 192),
        loading: Color32::from_rgb(115, 190, 255),
        tooltip_bg: Color32::from_rgba_premultiplied(16, 19, 26, 205),
        tooltip_border: Color32::from_rgba_premultiplied(110, 130, 160, 190),
        tooltip_text: Color32::from_rgba_premultiplied(250, 251, 255, 245),
    };

    pub const LIGHT: Self = Self {
        rail_bg: Color32::from_rgb(241, 243, 246),
        rail_surface: Color32::from_rgb(224, 228, 234),
        rail_row_active: Color32::from_rgb(211, 227, 250),
        rail_row_hover: Color32::from_rgb(231, 235, 241),
        rail_accent: Color32::from_rgb(44, 107, 237),
        text: Color32::from_rgb(31, 35, 40),
        text_dim: Color32::from_rgb(95, 102, 114),
        text_strong: Color32::from_rgb(17, 20, 24),
        brand: Color32::from_rgb(36, 92, 158),
        section_label: Color32::from_rgb(107, 114, 128),
        shield_ok: Color32::from_rgb(31, 138, 76),
        scrim: Color32::from_rgba_premultiplied(4, 5, 10, 60),
        palette_bg: Color32::from_rgb(255, 255, 255),
        palette_border: Color32::from_rgb(183, 193, 207),
        palette_subtext: Color32::from_rgb(95, 102, 114),
        palette_hint: Color32::from_rgb(122, 130, 142),
        palette_row_selected: Color32::from_rgb(210, 226, 250),
        palette_row_hover: Color32::from_rgb(235, 238, 243),
        menu_bg: Color32::from_rgb(255, 255, 255),
        menu_border: Color32::from_rgb(160, 175, 195),
        menu_disabled: Color32::from_rgb(150, 157, 168),
        close_bg: Color32::from_rgb(214, 220, 229),
        close_icon: Color32::from_rgb(17, 20, 24),
        tab_border: Color32::from_rgb(113, 155, 209),
        loading: Color32::from_rgb(44, 107, 237),
        tooltip_bg: Color32::from_rgba_premultiplied(255, 255, 255, 220),
        tooltip_border: Color32::from_rgba_premultiplied(150, 166, 188, 200),
        tooltip_text: Color32::from_rgba_premultiplied(25, 28, 33, 245),
    };

    /// Return a copy tinted by the active workspace accent. This keeps global
    /// theme constants immutable while letting workspaces recolour native
    /// chrome without ever touching CEF page content.
    pub fn with_workspace_accent(self, accent: Color32, light: bool) -> Self {
        let muted = if light {
            mix_color(accent, Color32::from_rgb(20, 28, 42), 0.32)
        } else {
            accent
        };
        Self {
            rail_row_active: mix_color(
                self.rail_row_active,
                accent,
                if light { 0.38 } else { 0.32 },
            ),
            rail_accent: muted,
            brand: muted,
            palette_border: mix_color(self.palette_border, accent, 0.45),
            palette_row_selected: mix_color(self.palette_row_selected, accent, 0.35),
            tab_border: muted,
            loading: muted,
            tooltip_border: mix_color(self.tooltip_border, accent, 0.4),
            ..self
        }
    }
}

fn mix_color(a: Color32, b: Color32, b_weight: f32) -> Color32 {
    let weight = b_weight.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_array();
    let [br, bg, bb, ba] = b.to_array();
    let mix = |from: u8, to: u8| (from as f32 * (1.0 - weight) + to as f32 * weight).round() as u8;
    Color32::from_rgba_premultiplied(mix(ar, br), mix(ag, bg), mix(ab, bb), mix(aa, ba))
}

impl ChromeTheme {
    pub fn palette(self) -> &'static ChromePalette {
        match self {
            Self::Dark => &ChromePalette::DARK,
            Self::Light => &ChromePalette::LIGHT,
        }
    }

    /// Map winit's optional system theme to a concrete chrome theme. Missing
    /// information keeps the established dark look instead of flashing light.
    pub fn from_winit(theme: Option<winit::window::Theme>) -> Self {
        match theme {
            Some(winit::window::Theme::Light) => Self::Light,
            Some(winit::window::Theme::Dark) | None => Self::Dark,
        }
    }

    /// Apply egui defaults so separators, hint text, scrollbars, and text
    /// cursors match the native palette. Explicit painted colours still come
    /// from `palette()`.
    pub fn apply_visuals(self, ctx: &egui::Context) {
        let palette = self.palette();
        let mut visuals = match self {
            Self::Dark => egui::Visuals::dark(),
            Self::Light => egui::Visuals::light(),
        };
        visuals.panel_fill = palette.rail_bg;
        visuals.window_fill = palette.palette_bg;
        visuals.extreme_bg_color = palette.rail_bg;
        visuals.faint_bg_color = palette.palette_row_hover;
        visuals.selection.bg_fill = palette.palette_row_selected;
        visuals.selection.stroke = Stroke::new(1.0_f32, palette.rail_accent);
        visuals.hyperlink_color = palette.rail_accent;
        visuals.override_text_color = Some(palette.text);
        ctx.set_visuals(visuals);
    }

    pub fn clear_color(self) -> wgpu::Color {
        match self {
            Self::Dark => wgpu::Color {
                r: 0.10,
                g: 0.11,
                b: 0.13,
                a: 1.0,
            },
            Self::Light => wgpu::Color {
                r: 0.945,
                g: 0.953,
                b: 0.965,
                a: 1.0,
            },
        }
    }
}

/// Resolve the user's appearance preference against the current OS theme.
/// Keeping this pure makes the Settings override easy to test.
pub fn effective_theme(preference: ThemePreference, system: ChromeTheme) -> ChromeTheme {
    match preference {
        ThemePreference::System => system,
        ThemePreference::Light => ChromeTheme::Light,
        ThemePreference::Dark => ChromeTheme::Dark,
    }
}

/// Human-readable label for a download's native state badge.
pub fn download_state_label(state: DownloadState) -> &'static str {
    match state {
        DownloadState::InProgress => "Downloading",
        DownloadState::Paused => "Paused",
        DownloadState::Completed => "Completed",
        DownloadState::Canceled => "Canceled",
        DownloadState::Interrupted => "Interrupted",
    }
}

/// Native segmented transfers display their connection count; CEF-managed
/// entries remain readable for non-HTTP downloads.
pub fn download_engine_label(engine: DownloadEngine, segments: u8) -> String {
    match engine {
        DownloadEngine::Cef => "CEF".to_string(),
        DownloadEngine::Parallel => {
            if segments > 1 {
                format!("Parallel · {segments} connections")
            } else {
                "Native".to_string()
            }
        }
    }
}

/// Compact binary sizes for the downloads bubble. Unknown totals stay `0 B`
/// instead of rendering misleading negative progress.
pub fn format_download_bytes(bytes: i64) -> String {
    if bytes <= 0 {
        return "0 B".to_string();
    }
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Estimated time remaining for a transfer. Unknown totals, finished transfers,
/// and paused/zero-speed transfers render nothing rather than a misleading ETA.
pub fn download_eta_label(received: i64, total: i64, speed: i64) -> String {
    if speed <= 0 || total <= 0 {
        return String::new();
    }
    let remaining = total.saturating_sub(received.max(0)).max(0);
    if remaining == 0 {
        return String::new();
    }
    let seconds = ((remaining as f64 / speed as f64).ceil() as u64)
        .max(1)
        .min(u64::MAX);
    let duration = if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    };
    format!("ETA {duration}")
}

/// Aggregate state for the downloads bubble header. Failed/canceled entries are
/// counted separately so a stale partial transfer cannot drag the overall bar
/// down forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DownloadSummary {
    pub active: usize,
    pub completed: usize,
    pub failed: usize,
    pub received: i64,
    pub total: i64,
    pub speed: i64,
}

impl DownloadSummary {
    pub fn from_downloads(downloads: &[DownloadInfo]) -> Self {
        let mut summary = Self::default();
        for download in downloads {
            match download.state {
                DownloadState::InProgress | DownloadState::Paused => summary.active += 1,
                DownloadState::Completed => summary.completed += 1,
                DownloadState::Canceled | DownloadState::Interrupted => summary.failed += 1,
            }
            if matches!(
                download.state,
                DownloadState::InProgress | DownloadState::Paused | DownloadState::Completed
            ) {
                summary.received = summary
                    .received
                    .saturating_add(download.received_bytes.max(0));
                summary.total = summary.total.saturating_add(download.total_bytes.max(0));
            }
            if download.state == DownloadState::InProgress {
                summary.speed = summary.speed.saturating_add(download.current_speed.max(0));
            }
        }
        summary
    }

    /// Integer percentage clamped to `0..=100`; unknown totals report `0`.
    pub fn percent(&self) -> u8 {
        if self.total <= 0 {
            return 0;
        }
        (self.received.saturating_mul(100) / self.total).clamp(0, 100) as u8
    }

    pub fn status_text(&self) -> String {
        let mut parts = Vec::new();
        if self.active > 0 {
            parts.push(format!("{} active", self.active));
        }
        if self.completed > 0 {
            parts.push(format!("{} done", self.completed));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if parts.is_empty() {
            "No downloads".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

/// Badge shown on the rail's Downloads icon. Running transfers get a count;
/// history with no running transfers gets a quiet dot so the icon still signals
/// that the bubble has something to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadBadge {
    Hidden,
    History,
    Active(usize),
}

pub fn download_rail_badge(downloads: &[DownloadInfo]) -> DownloadBadge {
    let active = downloads
        .iter()
        .filter(|download| {
            matches!(
                download.state,
                DownloadState::InProgress | DownloadState::Paused
            )
        })
        .count();
    if active > 0 {
        DownloadBadge::Active(active)
    } else if downloads.is_empty() {
        DownloadBadge::Hidden
    } else {
        DownloadBadge::History
    }
}

/// Completed and failed rows are intentionally static: they no longer display
/// a progress bar, so the row can show only its final state and file actions.
pub fn download_row_uses_progress_bar(state: DownloadState) -> bool {
    matches!(state, DownloadState::InProgress | DownloadState::Paused)
}

/// Translate the two independent workspace forms into actions. The edit form
/// only emits on an explicit Save click, and Create only reads the create form;
/// this prevents a half-finished rename from becoming a new workspace name.
fn workspace_form_actions(
    selected_workspace: Option<&WorkspaceInfo>,
    edit_name: &str,
    edit_icon: WorkspaceIcon,
    edit_color: WorkspaceColor,
    save_clicked: bool,
    new_name: &str,
    new_icon: WorkspaceIcon,
    new_color: WorkspaceColor,
    create_clicked: bool,
    delete_clicked: bool,
) -> Vec<UiAction> {
    let mut actions = Vec::new();

    if save_clicked {
        if let Some(workspace) = selected_workspace {
            let name = Workspace::normalize_name(edit_name);
            if name != workspace.name
                || edit_icon != workspace.icon
                || edit_color != workspace.color
            {
                actions.push(UiAction::UpdateWorkspace {
                    workspace_id: workspace.id,
                    name,
                    icon: edit_icon,
                    color: edit_color,
                });
            }
        }
    }

    if create_clicked {
        actions.push(UiAction::CreateWorkspace {
            name: Workspace::normalize_name(new_name),
            icon: new_icon,
            color: new_color,
        });
    }

    if delete_clicked {
        if let Some(workspace) = selected_workspace {
            actions.push(UiAction::DeleteWorkspace {
                workspace_id: workspace.id,
            });
        }
    }

    actions
}

const TOOLTIP_WIDTH: f32 = 190.0;
const TOOLTIP_HEIGHT_EXPANDED: f32 = 44.0;
const TOOLTIP_HEIGHT_COLLAPSED: f32 = 38.0;
const RAIL_MENU_WIDTH: f32 = 224.0;
const RAIL_MENU_ROW_HEIGHT: f32 = 28.0;
const RAIL_MENU_PADDING: f32 = 7.0;
const RAIL_STATUS_HEIGHT: f32 = 196.0;
const RAIL_STATUS_ICON_EXPANDED: f32 = 15.0;
const RAIL_STATUS_ICON_COLLAPSED: f32 = 20.0;
const RAIL_STATUS_BADGE_HEIGHT: f32 = 24.0;
const SETTINGS_WIDTH: f32 = 660.0;
const SETTINGS_MAX_CONTENT_HEIGHT: f32 = 560.0;
const DOWNLOADS_WIDTH: f32 = 460.0;
const DOWNLOADS_MAX_HEIGHT: f32 = 460.0;
const DOWNLOADS_BOTTOM_MARGIN: f32 = 18.0;
const DOWNLOADS_RAIL_GAP: f32 = 12.0;

/// Prefer egui's recommended non-sRGB surface formats. Some drivers only list
/// `Bgra8UnormSrgb` first, which makes `egui_wgpu` warn about linear sRGB
/// framebuffers; choosing the matching `Unorm` format avoids the warning.
fn preferred_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    formats
        .iter()
        .copied()
        .find(|format| *format == wgpu::TextureFormat::Rgba8Unorm)
        .or_else(|| {
            formats
                .iter()
                .copied()
                .find(|format| *format == wgpu::TextureFormat::Bgra8Unorm)
        })
        .or_else(|| formats.iter().copied().find(|format| !format.is_srgb()))
        .or_else(|| formats.first().copied())
}

/// Semantic icons mapped to the registered Material Symbols font. Keeping a
/// small enum prevents glyph constants from leaking into the layout code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailIcon {
    Plus,
    Search,
    Code,
    ChevronLeft,
    ChevronRight,
    Moon,
    Shield,
    Chip,
    Globe,
    Close,
    Pin,
    PinOff,
    Copy,
    Sidebar,
    Settings,
    Download,
    Pause,
    Play,
    Retry,
    FolderOpen,
    Check,
    Delete,
    Star,
    Edit,
    BookmarkAdd,
    OpenInNew,
    WorkspaceWork,
    WorkspaceCode,
    WorkspaceStudy,
    WorkspaceLaunch,
    WorkspaceDesign,
    WorkspaceMusic,
    WorkspaceReading,
    WorkspaceWeb,
}

impl From<WorkspaceIcon> for RailIcon {
    fn from(icon: WorkspaceIcon) -> Self {
        match icon {
            WorkspaceIcon::Work => Self::WorkspaceWork,
            WorkspaceIcon::Code => Self::WorkspaceCode,
            WorkspaceIcon::School => Self::WorkspaceStudy,
            WorkspaceIcon::Rocket => Self::WorkspaceLaunch,
            WorkspaceIcon::Palette => Self::WorkspaceDesign,
            WorkspaceIcon::Music => Self::WorkspaceMusic,
            WorkspaceIcon::Book => Self::WorkspaceReading,
            WorkspaceIcon::Public => Self::WorkspaceWeb,
        }
    }
}

/// Emphasis level for native buttons that use the Material Symbols font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaterialButtonStyle {
    Filled,
    Tonal,
    Outlined,
    Text,
}

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Snapshot of a single tab's metadata used by the chrome renderer.
#[derive(Debug, Clone, PartialEq)]
pub enum TabDisplayState {
    Active,
    Background,
    Hibernated,
}

/// Decoded favicon pixels delivered from the CEF favicon download callback.
#[derive(Debug, Clone)]
pub struct Favicon {
    pub url: String,
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct TabInfo {
    pub id: u64,
    pub workspace_id: u64,
    pub workspace_name: String,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub display_state: TabDisplayState,
    pub blocked_requests: u32,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub is_pinned: bool,
    pub favicon: Option<Favicon>,
}

/// A native workspace as displayed in the rail and settings overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub name: String,
    pub icon: WorkspaceIcon,
    pub color: WorkspaceColor,
    pub is_default: bool,
    pub is_active: bool,
}

/// A globally visible bookmark grouped by its workspace-named folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkInfo {
    pub id: i64,
    pub workspace_id: u64,
    pub url: String,
    pub title: String,
    pub folder: String,
}

/// An action produced by selecting a command-palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    SelectTab(u64),
    NewTab(Option<String>),
    Navigate { tab_id: u64, url: String },
    TogglePinTab(u64),
    HibernateAll,
    ToggleRail,
    OpenDevTools,
    OpenSettings,
    OpenDownloads,
}

/// One row in the floating command palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPaletteItem {
    pub icon: &'static str,
    pub label: String,
    pub subtitle: String,
    pub action: CommandAction,
}

/// A rail tooltip to draw in a transparent overlay above the CEF page.
#[derive(Debug, Clone, PartialEq)]
pub struct RailTooltip {
    pub text: String,
    /// Top-left corner in main-window logical coordinates.
    pub anchor: egui::Pos2,
    pub width: f32,
    pub height: f32,
}

/// A native tab context menu drawn in a transparent overlay above the page.
#[derive(Debug, Clone, PartialEq)]
pub struct RailMenu {
    pub tab_id: u64,
    /// Top-left corner in main-window logical coordinates.
    pub anchor: egui::Pos2,
    pub width: f32,
    pub height: f32,
}

/// Commands offered by the rail tab context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailMenuAction {
    NewTab,
    DuplicateTab,
    TogglePinTab,
    CopyUrl,
    Reload,
    CloseOtherTabs,
    CloseTabsToRight,
    ReopenClosedTab,
    CloseTab,
    OpenDevTools,
}

/// Result produced by the menu overlay on each interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailMenuOutcome {
    Dismiss,
    Action(UiAction),
}

/// Page selected inside the native Settings dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Workspaces,
    Bookmarks,
}

/// One row in the rail tab context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailMenuItem {
    pub label: &'static str,
    pub action: RailMenuAction,
    pub enabled: bool,
}

/// All mutable UI state consumed by `ChromeRenderer::render()` each frame.
pub struct ChromeState {
    pub tabs: Vec<TabInfo>,
    pub active_tab_id: Option<u64>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub active_workspace_id: u64,
    pub bookmarks: Vec<BookmarkInfo>,
    /// WAL-backed download snapshots mirrored from the CEF download handler.
    pub downloads: Vec<DownloadInfo>,
    /// Text mirrored from the active tab; the command palette edits a copy.
    pub omnibox_text: String,
    /// True while the command palette is editing `omnibox_text`.
    pub omnibox_focused: bool,
    /// Tab whose URL was last copied into the omnibox.
    pub last_synced_tab_id: Option<u64>,
    /// URL that was last copied into the omnibox.
    pub last_synced_url: String,
    /// Whether the active tab is currently loading.
    pub is_loading: bool,
    /// RAM used by this process in megabytes (updated by sysinfo watchdog).
    pub ram_mb: u64,
    /// Whether the vertical workspace rail is expanded.
    pub rail_expanded: bool,
    /// Whether the floating Cmd/Ctrl+K palette is visible.
    pub command_palette_open: bool,
    /// Current query in the command palette.
    pub command_palette_query: String,
    /// Currently highlighted command palette row.
    pub command_selection: usize,
    /// Set when the palette should focus its query input on next render.
    pub command_palette_focus_requested: bool,
    /// Set when the focused palette input should select all text on next render.
    /// Selection is deferred one frame because egui processes key events before
    /// a widget that requests focus during that same frame exists.
    pub command_palette_select_all_requested: bool,
    /// Latest rail tooltip; consumed by the overlay window on each frame.
    pub tooltip: Option<RailTooltip>,
    /// Open tab context menu; consumed by the shared full-window overlay.
    pub menu: Option<RailMenu>,
    /// Currently highlighted context-menu row.
    pub menu_selection: usize,
    /// True while the native downloads bubble is visible in the overlay.
    pub downloads_open: bool,
    /// True when at least one closed tab can be restored with Cmd/Ctrl+Z.
    pub can_reopen_tab: bool,
    /// Current OS light/dark appearance. The user preference may override it.
    pub system_theme: ChromeTheme,
    /// Effective appearance used by every native chrome surface.
    pub theme: ChromeTheme,
    /// Persisted user preferences mirrored into the chrome state.
    pub preferences: BrowserPreferences,
    /// True while the native Settings overlay is visible.
    pub settings_open: bool,
    /// Page currently shown in the Settings overlay.
    pub settings_tab: SettingsTab,
    /// Working copy edited by the Settings overlay; committed on every change.
    pub settings_draft: BrowserPreferences,
    /// Workspace currently selected in the Settings editor.
    pub selected_workspace_id: Option<u64>,
    /// Separate draft for renaming/recolouring the selected workspace.
    pub workspace_edit_name_draft: String,
    pub workspace_edit_icon_draft: WorkspaceIcon,
    pub workspace_edit_color_draft: WorkspaceColor,
    /// When true, the Workspaces page shows only the dedicated create form.
    /// Manage mode never shows create fields beside an existing workspace.
    pub workspace_create_mode: bool,
    /// Draft used only while `workspace_create_mode` is active. Keeping it
    /// distinct from the editor prevents "Create" from renaming the selected
    /// workspace.
    pub workspace_new_name_draft: String,
    pub workspace_new_icon_draft: WorkspaceIcon,
    pub workspace_new_color_draft: WorkspaceColor,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab_id: None,
            workspaces: Vec::new(),
            active_workspace_id: 1,
            bookmarks: Vec::new(),
            downloads: Vec::new(),
            omnibox_text: String::new(),
            omnibox_focused: false,
            last_synced_tab_id: None,
            last_synced_url: String::new(),
            is_loading: false,
            ram_mb: 0,
            // Start visible so the vertical layout is immediately discoverable;
            // the ‹ button collapses it to a favicon-only rail.
            rail_expanded: true,
            command_palette_open: false,
            command_palette_query: String::new(),
            command_selection: 0,
            command_palette_focus_requested: false,
            command_palette_select_all_requested: false,
            tooltip: None,
            menu: None,
            menu_selection: 0,
            downloads_open: false,
            can_reopen_tab: false,
            system_theme: ChromeTheme::default(),
            theme: ChromeTheme::default(),
            preferences: BrowserPreferences::default(),
            settings_open: false,
            settings_tab: SettingsTab::General,
            settings_draft: BrowserPreferences::default(),
            selected_workspace_id: None,
            workspace_edit_name_draft: String::new(),
            workspace_edit_icon_draft: WorkspaceIcon::default(),
            workspace_edit_color_draft: WorkspaceColor::default(),
            workspace_create_mode: false,
            workspace_new_name_draft: "New workspace".to_string(),
            workspace_new_icon_draft: WorkspaceIcon::default(),
            workspace_new_color_draft: WorkspaceColor::default(),
        }
    }
}

impl ChromeState {
    /// Open the command palette seeded with the active tab's current address.
    pub fn open_command_palette(&mut self) {
        if !self.command_palette_open {
            self.command_palette_query = self.omnibox_text.clone();
            self.command_selection = 0;
            self.command_palette_focus_requested = true;
            self.command_palette_select_all_requested = false;
        }
        self.command_palette_open = true;
        self.close_tab_menu();
        self.settings_open = false;
        self.downloads_open = false;
    }

    pub fn close_command_palette(&mut self) {
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.tooltip = None;
        self.close_tab_menu();
    }

    pub fn toggle_rail(&mut self, actions: &mut Vec<UiAction>) {
        self.rail_expanded = !self.rail_expanded;
        self.close_tab_menu();
        actions.push(UiAction::SetRailExpanded(self.rail_expanded));
    }

    pub fn open_tab_menu(&mut self, tab_id: u64, anchor: egui::Pos2) {
        // The palette and menu share one transparent overlay window and must
        // never both claim it. Clearing the palette here also avoids a stale
        // focus request when the menu is dismissed.
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;

        let (width, height) = rail_menu_metrics();
        self.menu = Some(RailMenu {
            tab_id,
            anchor,
            width,
            height,
        });
        self.menu_selection = 0;
        self.tooltip = None;
        self.settings_open = false;
        self.downloads_open = false;
    }

    pub fn close_tab_menu(&mut self) {
        self.menu = None;
        self.menu_selection = 0;
    }

    /// Open the native Settings overlay on the General page.
    pub fn open_settings(&mut self) {
        self.open_settings_tab(SettingsTab::General);
    }

    /// Open the Arc/Edge-style downloads bubble in the shared overlay.
    pub fn open_downloads(&mut self) {
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.close_tab_menu();
        self.tooltip = None;
        self.settings_open = false;
        self.downloads_open = true;
    }

    pub fn toggle_downloads(&mut self) {
        if self.downloads_open {
            self.close_downloads();
        } else {
            self.open_downloads();
        }
    }

    pub fn close_downloads(&mut self) {
        self.downloads_open = false;
        self.tooltip = None;
    }

    /// Open the native Settings overlay on a specific page, seeded from
    /// committed preferences. Workspace controls get their own page so the
    /// dialog stays manageable as features are added.
    pub fn open_settings_tab(&mut self, tab: SettingsTab) {
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.close_tab_menu();
        self.tooltip = None;
        self.downloads_open = false;
        self.settings_tab = tab;
        self.settings_draft = self.preferences.clone();
        self.workspace_create_mode = false;
        self.reset_workspace_create_draft();
        self.sync_workspace_draft();
        self.settings_open = true;
    }

    /// Open Workspaces directly in the dedicated creation form. The rail's
    /// **New workspace** button uses this so creation is never mixed into the
    /// list of existing workspaces.
    pub fn open_workspace_creator(&mut self) {
        self.open_settings_tab(SettingsTab::Workspaces);
        self.workspace_create_mode = true;
    }

    fn reset_workspace_create_draft(&mut self) {
        self.workspace_new_name_draft = "New workspace".to_string();
        self.workspace_new_icon_draft = WorkspaceIcon::default();
        self.workspace_new_color_draft = WorkspaceColor::default();
    }

    /// Leave creation mode without emitting an action. Used by Cancel/Back.
    pub fn close_workspace_creator(&mut self) {
        self.workspace_create_mode = false;
        self.reset_workspace_create_draft();
    }

    /// Re-seed the editor from whichever workspace becomes active after a
    /// create/delete action. The next frame has fresh workspace metadata by
    /// then, so the newly created or remaining workspace is selected.
    pub fn follow_active_workspace(&mut self) {
        self.selected_workspace_id = None;
        self.workspace_edit_name_draft.clear();
        self.workspace_edit_icon_draft = WorkspaceIcon::default();
        self.workspace_edit_color_draft = WorkspaceColor::default();
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_draft = self.preferences.clone();
        self.workspace_create_mode = false;
        self.reset_workspace_create_draft();
    }

    /// Close whichever shared overlay is currently visible.
    pub fn close_active_overlay(&mut self) {
        if self.settings_open {
            self.close_settings();
        } else if self.menu.is_some() {
            self.close_tab_menu();
        } else if self.command_palette_open {
            self.close_command_palette();
        } else if self.downloads_open {
            self.close_downloads();
        }
    }

    pub fn has_overlay(&self) -> bool {
        self.command_palette_open
            || self.menu.is_some()
            || self.settings_open
            || self.downloads_open
    }

    /// Update the OS appearance and recompute the effective chrome theme.
    pub fn set_system_theme(&mut self, system: ChromeTheme) {
        self.system_theme = system;
        self.refresh_theme();
    }

    /// Install preference changes and immediately update derived UI state.
    ///
    /// The rail's `rail_expanded` preference is a *startup default*, not a live
    /// mirror of the current rail. The winit handler applies it only when the
    /// preference itself changes, so editing an unrelated setting never
    /// expands a rail the user collapsed manually.
    pub fn set_preferences(&mut self, preferences: BrowserPreferences) {
        let preferences = preferences.normalized();
        self.preferences = preferences.clone();
        self.settings_draft = preferences;
        self.refresh_theme();
    }

    fn refresh_theme(&mut self) {
        self.theme = effective_theme(self.preferences.theme, self.system_theme);
    }

    /// Palette for this frame, tinted by the active workspace's accent colour.
    pub fn palette(&self) -> ChromePalette {
        let accent = self
            .active_workspace()
            .map(|workspace| workspace.color)
            .unwrap_or_default();
        let (r, g, b) = accent.rgb();
        self.theme
            .palette()
            .with_workspace_accent(Color32::from_rgb(r, g, b), self.theme == ChromeTheme::Light)
    }

    /// Apply chrome visuals plus the live workspace accent to egui widgets.
    pub fn apply_visuals(&self, ctx: &egui::Context) {
        let base = self.theme.palette();
        let palette = self.palette();
        let mut visuals = match self.theme {
            ChromeTheme::Dark => egui::Visuals::dark(),
            ChromeTheme::Light => egui::Visuals::light(),
        };
        visuals.panel_fill = base.rail_bg;
        visuals.window_fill = base.palette_bg;
        visuals.extreme_bg_color = base.rail_bg;
        visuals.faint_bg_color = base.palette_row_hover;
        visuals.selection.bg_fill = palette.palette_row_selected;
        visuals.selection.stroke = Stroke::new(1.0_f32, palette.rail_accent);
        visuals.hyperlink_color = palette.rail_accent;
        visuals.override_text_color = Some(base.text);
        ctx.set_visuals(visuals);
    }

    pub fn active_workspace(&self) -> Option<&WorkspaceInfo> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == self.active_workspace_id)
    }

    /// Refresh the Settings editor from the selected workspace, falling back to
    /// the active workspace when metadata changed underneath us. Skipped while
    /// Settings is open so typing is never clobbered by the next frame's
    /// tab-manager sync.
    pub fn sync_workspace_draft(&mut self) {
        if self.settings_open {
            return;
        }
        let selected = self
            .workspaces
            .iter()
            .find(|workspace| Some(workspace.id) == self.selected_workspace_id)
            .or_else(|| self.active_workspace())
            .or_else(|| self.workspaces.first())
            .cloned();
        self.selected_workspace_id = selected.as_ref().map(|workspace| workspace.id);
        if let Some(workspace) = selected {
            self.workspace_edit_name_draft = workspace.name;
            self.workspace_edit_icon_draft = workspace.icon;
            self.workspace_edit_color_draft = workspace.color;
        }
    }

    /// Select a workspace in the Settings editor without touching the separate
    /// new-workspace form.
    pub fn select_workspace_for_editing(&mut self, workspace_id: u64) {
        let Some(workspace) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned()
        else {
            return;
        };
        self.selected_workspace_id = Some(workspace.id);
        self.workspace_edit_name_draft = workspace.name;
        self.workspace_edit_icon_draft = workspace.icon;
        self.workspace_edit_color_draft = workspace.color;
    }
}

// ---------------------------------------------------------------------------
// ChromeRenderer
// ---------------------------------------------------------------------------

/// Owns the wgpu surface + egui renderer for the native browser chrome.
pub struct ChromeRenderer {
    surface: wgpu::Surface<'static>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    /// Tracks raw input state across frames.
    egui_state: egui_winit::State,
    /// Cached egui textures keyed by favicon URL.
    favicon_textures: HashMap<String, egui::TextureHandle>,
    /// Max texture dimension for this adapter; used to clamp surface size.
    max_texture_dim: u32,
}

impl ChromeRenderer {
    /// Create a renderer bound to `window`.  Must be called from the main thread
    /// after wgpu is available.
    pub fn new(
        window: Arc<winit::window::Window>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> Option<Self> {
        let surface = match instance.create_surface(Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(e) => {
                tracing::error!("Failed to create chrome surface: {e}");
                return None;
            }
        };

        let size = window.inner_size();
        let caps = surface.get_capabilities(adapter);
        let Some(format) = preferred_surface_format(&caps.formats) else {
            tracing::error!("Chrome adapter reported no surface formats");
            return None;
        };
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Opaque);

        // Clamp surface dimensions to the adapter's max texture size.
        let max_tex = adapter.limits().max_texture_dimension_2d;
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1).min(max_tex),
            height: size.height.max(1).min(max_tex),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        let egui_ctx = egui::Context::default();
        // Register the Material Symbols fallback used by every rail icon. It
        // is pinned to the egui 0.30-compatible crate release.
        egui_material_icons::initialize(&egui_ctx);
        // Tooltips must appear immediately; the default delay makes the chrome
        // controls look unlabeled during quick hovers.
        egui_ctx.style_mut(|style| {
            style.interaction.tooltip_delay = 0.0;
        });

        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(max_tex as usize),
        );

        Some(Self {
            surface,
            device,
            queue,
            surface_config,
            renderer,
            egui_ctx,
            egui_state,
            favicon_textures: HashMap::new(),
            max_texture_dim: max_tex,
        })
    }

    /// Forward a winit `WindowEvent` to egui for input processing.
    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
        let resp = self.egui_state.on_window_event(window, event);
        resp.consumed
    }

    /// Resize the wgpu surface when the window size changes.
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.surface_config.width = new_size.width.min(self.max_texture_dim);
        self.surface_config.height = new_size.height.min(self.max_texture_dim);
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Render one frame of chrome UI.  Returns the list of actions the user triggered.
    pub fn render(
        &mut self,
        window: &winit::window::Window,
        state: &mut ChromeState,
    ) -> Vec<UiAction> {
        let mut actions: Vec<UiAction> = Vec::new();

        self.sync_favicon_textures(state);
        state.apply_visuals(&self.egui_ctx);

        let raw_input = self.egui_state.take_egui_input(window);
        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            self.build_ui(ctx, state, &mut actions);
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        let output_frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("wgpu surface error: {:?}", e);
                return actions;
            }
        };
        let view = output_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: window.scale_factor() as f32,
        };

        let tris = self
            .egui_ctx
            .tessellate(full_output.shapes, screen_descriptor.pixels_per_point);

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("chrome"),
            });
        self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &tris,
            &screen_descriptor,
        );

        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("chrome_rpass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(state.theme.clear_color()),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output_frame.present();

        actions
    }

    /// Install any newly downloaded favicon bitmaps into the egui texture cache.
    fn sync_favicon_textures(&mut self, state: &ChromeState) {
        let mut live_urls = HashSet::new();

        for tab in &state.tabs {
            let Some(favicon) = &tab.favicon else {
                continue;
            };
            if favicon.width == 0 || favicon.height == 0 || favicon.rgba.is_empty() {
                continue;
            }

            live_urls.insert(favicon.url.clone());
            if self.favicon_textures.contains_key(&favicon.url) {
                continue;
            }

            let image = egui::ColorImage::from_rgba_unmultiplied(
                [favicon.width as usize, favicon.height as usize],
                &favicon.rgba[..],
            );
            let texture =
                self.egui_ctx
                    .load_texture(favicon.url.clone(), image, TextureOptions::LINEAR);
            self.favicon_textures.insert(favicon.url.clone(), texture);
        }

        self.favicon_textures
            .retain(|url, _| live_urls.contains(url));
    }

    // -----------------------------------------------------------------------
    // UI construction
    // -----------------------------------------------------------------------

    fn build_ui(&self, ctx: &egui::Context, state: &mut ChromeState, actions: &mut Vec<UiAction>) {
        self.build_rail(ctx, state, actions);

        // The CEF view occupies everything to the right of the rail. The
        // central panel is transparent so it never paints over page content.
        egui::CentralPanel::default()
            .frame(Frame::none().fill(Color32::TRANSPARENT))
            .show(ctx, |_ui| {});
    }

    /// Draw the workspace strip at the top of the rail. Workspaces switch the
    /// visible tab set and accent colour; the plus button opens Settings where
    /// a new workspace can be named and customised.
    fn build_workspace_switcher(
        ui: &mut Ui,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        tooltip_slot: &mut Option<RailTooltip>,
        palette: &ChromePalette,
        width: f32,
        expanded: bool,
    ) {
        let workspaces = state.workspaces.clone();
        if workspaces.is_empty() {
            return;
        }

        if !expanded {
            // The collapsed rail is the primary workspace switcher, so each
            // space gets a large, high-contrast target with an explicit accent
            // border and a name tooltip instead of a tiny colour dot.
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                for (index, workspace) in workspaces.iter().enumerate() {
                    if index > 0 {
                        ui.add_space(6.0);
                    }
                    let active = workspace.id == state.active_workspace_id;
                    let (rect, response) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
                    let (r, g, b) = workspace.color.rgb();
                    let accent = Color32::from_rgb(r, g, b);
                    let fill = if active {
                        mix_color(palette.rail_surface, accent, 0.42)
                    } else if response.hovered() {
                        palette.rail_row_hover
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(rect, Rounding::same(9.0), fill);
                    ui.painter().rect_stroke(
                        rect.shrink(0.5),
                        Rounding::same(9.0),
                        Stroke::new(
                            if active { 2.0_f32 } else { 1.0_f32 },
                            if active {
                                accent
                            } else {
                                palette.palette_border
                            },
                        ),
                    );
                    paint_rail_icon(
                        ui,
                        rect.shrink(9.0),
                        workspace.icon.into(),
                        if active {
                            palette.text_strong
                        } else {
                            palette.text
                        },
                    );
                    if active {
                        ui.painter().circle_filled(
                            pos2(rect.center().x, rect.max.y - 4.0),
                            2.0,
                            accent,
                        );
                    }
                    let tooltip = if active {
                        format!("{} — current space", workspace.name)
                    } else if workspace.is_default {
                        format!("Switch to {} (default)", workspace.name)
                    } else {
                        format!("Switch to {}", workspace.name)
                    };
                    Self::chrome_tooltip(tooltip_slot, ui, &response, tooltip, false);
                    if response.clicked() {
                        actions.push(UiAction::SwitchWorkspace {
                            workspace_id: workspace.id,
                        });
                    }
                }
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new("SPACES")
                    .size(9.0)
                    .color(palette.section_label),
            );
        });
        ui.add_space(2.0);
        for workspace in &workspaces {
            let active = workspace.id == state.active_workspace_id;
            let (rect, response) = ui.allocate_exact_size(vec2(width - 12.0, 26.0), Sense::click());
            let (r, g, b) = workspace.color.rgb();
            let color = Color32::from_rgb(r, g, b);
            let fill = if active {
                mix_color(palette.rail_surface, color, 0.34)
            } else if response.hovered() {
                palette.rail_row_hover
            } else {
                Color32::TRANSPARENT
            };
            ui.painter().rect_filled(rect, Rounding::same(7.0), fill);
            if active {
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    Rounding::same(7.0),
                    Stroke::new(1.0_f32, color),
                );
            }
            let icon_rect = egui::Rect::from_center_size(
                pos2(rect.min.x + 16.0, rect.center().y),
                vec2(16.0, 16.0),
            );
            paint_rail_icon(ui, icon_rect, workspace.icon.into(), color);
            ui.painter().text(
                pos2(rect.min.x + 30.0, rect.center().y),
                Align2::LEFT_CENTER,
                truncate_title(&workspace.name, 14),
                FontId::new(11.0, FontFamily::Proportional),
                if active {
                    palette.text_strong
                } else {
                    palette.text
                },
            );
            if response.clicked() {
                actions.push(UiAction::SwitchWorkspace {
                    workspace_id: workspace.id,
                });
            }
            ui.add_space(3.0);
        }

        let add = Self::rail_action_button(
            ui,
            width - 12.0,
            26.0,
            RailIcon::Plus,
            "New workspace",
            palette,
        );
        if add.clicked() {
            // Open the dedicated create form instead of mixing creation into
            // the list of existing workspaces.
            state.open_workspace_creator();
        }
    }

    /// Draw the framed URL field in the expanded rail.
    ///
    /// `TextEdit` allocates hidden space for long single-line text, so it is
    /// wrapped in a width-clamped scroll area. Explicit spacing and a zero text
    /// margin keep the surrounding frame inside the rail with even side gaps.
    fn build_rail_address_field(
        ui: &mut Ui,
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        palette: &ChromePalette,
    ) -> Response {
        Frame::none()
            .fill(palette.rail_surface)
            .stroke(Stroke::new(1.0_f32, palette.palette_border))
            .rounding(Rounding::same(9.0))
            .outer_margin(egui::Margin::symmetric(6.0, 0.0))
            .inner_margin(egui::Margin::symmetric(8.0, 6.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Explicit spaces below define the gaps; egui's implicit
                    // item spacing would otherwise add 8 px between each glyph.
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (icon_rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                    paint_rail_icon(ui, icon_rect, RailIcon::Globe, palette.rail_accent);
                    ui.add_space(4.0);

                    // The 22 px copy slot includes its 4 px leading gap. The
                    // scroll area keeps TextEdit's hidden-text allocation from
                    // expanding the frame, even for very long URLs.
                    let copy_slot_width = 22.0;
                    let input_width = (ui.available_width() - copy_slot_width).max(24.0);
                    let url_input = egui::ScrollArea::horizontal()
                        .id_salt("kasumi_rail_url_scroll")
                        .max_width(input_width)
                        .scroll_bar_visibility(
                            egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden,
                        )
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::singleline(&mut state.omnibox_text)
                                    .id(egui::Id::new("kasumi_rail_url"))
                                    .hint_text("Address or search")
                                    .text_color(palette.text_strong)
                                    .font(egui::TextStyle::Body)
                                    .desired_width(input_width)
                                    .clip_text(true)
                                    .margin(egui::Margin::ZERO)
                                    .frame(false),
                            )
                        })
                        .inner;
                    if url_input.has_focus() {
                        state.omnibox_focused = true;
                    } else if state.omnibox_focused && !state.command_palette_open {
                        state.omnibox_focused = false;
                    }
                    if url_input.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Some(tab_id) = state.active_tab_id {
                            let url = resolve_omnibox_with_search(
                                &state.omnibox_text,
                                state.preferences.search_url_prefix(),
                            );
                            actions.push(UiAction::Navigate { tab_id, url });
                        }
                    }

                    let (copy_rect, copy_response) =
                        ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    paint_rail_icon(
                        ui,
                        egui::Rect::from_center_size(copy_rect.center(), vec2(12.0, 12.0)),
                        RailIcon::Copy,
                        palette.text,
                    );
                    if copy_response.clicked() {
                        ctx.copy_text(state.omnibox_text.clone());
                    }
                    copy_response.on_hover_text("Copy URL");
                });
            })
            .response
    }

    /// Draw the collapsible left workspace rail.
    fn build_rail(
        &self,
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        // Tooltips are rendered in a transparent overlay above the page, so
        // collect the hovered row for this frame instead of painting in-rail.
        let mut rail_tooltip: Option<RailTooltip> = None;
        let mut menu_request: Option<(u64, egui::Pos2)> = None;
        // Pass a borrow into the panel closure so the collected tooltip can be
        // read again after the closure finishes.
        let tooltip_slot = &mut rail_tooltip;
        let width = rail_width(state.rail_expanded);
        let palette = state.palette();
        let palette = &palette;
        // Computed once so both rail modes show the same active-download state.
        let download_badge = download_rail_badge(&state.downloads);
        let rail = egui::SidePanel::left("workspace_rail")
            .exact_width(width)
            .resizable(false)
            .frame(Frame::none().fill(palette.rail_bg))
            .show(ctx, |ui| {
                if !state.rail_expanded {
                    // Favicon-only mode: show one large, clickable icon per tab,
                    // plus always-visible expand / new-tab / command controls.
                    ui.with_layout(Layout::top_down(Align::Center), |ui| {
                        let rail_rect = ui.clip_rect();
                        ui.add_space(5.0);

                        Self::build_workspace_switcher(
                            ui,
                            state,
                            actions,
                            tooltip_slot,
                            &palette,
                            width,
                            false,
                        );
                        ui.add_space(4.0);

                        ui.add_space(3.0);
                        let new_tab =
                            Self::rail_icon_button(ui, vec2(34.0, 34.0), RailIcon::Plus, palette);
                        if new_tab.clicked() {
                            actions.push(UiAction::NewTab { url: None });
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &new_tab,
                            "New tab (Cmd/Ctrl+T)",
                            state.rail_expanded,
                        );

                        ui.add_space(2.0);
                        let command =
                            Self::rail_icon_button(ui, vec2(34.0, 30.0), RailIcon::Search, palette);
                        if command.clicked() {
                            state.open_command_palette();
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &command,
                            "Command palette (Cmd/Ctrl+K)",
                            state.rail_expanded,
                        );

                        ui.add_space(2.0);
                        let devtools =
                            Self::rail_icon_button(ui, vec2(34.0, 30.0), RailIcon::Code, palette);
                        if devtools.clicked() {
                            if let Some(tab_id) = state.active_tab_id {
                                actions.push(UiAction::OpenDevTools { tab_id });
                            }
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &devtools,
                            "DevTools (F12)",
                            state.rail_expanded,
                        );

                        ui.add_space(5.0);
                        ui.separator();
                        ui.add_space(5.0);

                        let tab_area_height =
                            (ui.available_height() - RAIL_STATUS_HEIGHT - 6.0).max(44.0);
                        egui::ScrollArea::vertical()
                            .id_salt("favicon_rail_tabs")
                            .max_height(tab_area_height)
                            .show(ui, |ui| {
                                for tab in state.tabs.iter().filter(|tab| {
                                    tab.workspace_id == state.active_workspace_id && tab.is_pinned
                                }) {
                                    self.rail_tab_row(
                                        ui,
                                        tab,
                                        state,
                                        actions,
                                        tooltip_slot,
                                        &mut menu_request,
                                        rail_rect,
                                        palette,
                                        width,
                                        false,
                                    );
                                }
                                if state.tabs.iter().any(|tab| {
                                    tab.workspace_id == state.active_workspace_id && tab.is_pinned
                                }) && state.tabs.iter().any(|tab| {
                                    tab.workspace_id == state.active_workspace_id && !tab.is_pinned
                                }) {
                                    ui.add_space(2.0);
                                    ui.separator();
                                    ui.add_space(2.0);
                                }
                                for tab in state.tabs.iter().filter(|tab| {
                                    tab.workspace_id == state.active_workspace_id && !tab.is_pinned
                                }) {
                                    self.rail_tab_row(
                                        ui,
                                        tab,
                                        state,
                                        actions,
                                        tooltip_slot,
                                        &mut menu_request,
                                        rail_rect,
                                        palette,
                                        width,
                                        false,
                                    );
                                }
                            });

                        // The collapsed rail keeps the same bottom action
                        // block as the expanded rail, using icon-only buttons.
                        let status_rect = egui::Rect::from_min_max(
                            pos2(0.0, ui.clip_rect().bottom() - RAIL_STATUS_HEIGHT),
                            pos2(width, ui.clip_rect().bottom()),
                        );
                        ui.allocate_new_ui(UiBuilder::new().max_rect(status_rect), |ui| {
                            ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                                ui.add_space(5.0);
                                let expand = Self::rail_icon_button(
                                    ui,
                                    vec2(34.0, 30.0),
                                    RailIcon::Sidebar,
                                    palette,
                                );
                                if expand.clicked() {
                                    state.toggle_rail(actions);
                                }
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &expand,
                                    "Expand workspace rail",
                                    state.rail_expanded,
                                );

                                ui.add_space(4.0);
                                let hibernate = Self::rail_icon_button(
                                    ui,
                                    vec2(34.0, 30.0),
                                    RailIcon::Moon,
                                    palette,
                                );
                                if hibernate.clicked() {
                                    actions.push(UiAction::HibernateAllBackground);
                                }
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &hibernate,
                                    "Hibernate tabs (Alt+H)",
                                    state.rail_expanded,
                                );

                                ui.add_space(4.0);
                                let settings = Self::rail_icon_button(
                                    ui,
                                    vec2(34.0, 30.0),
                                    RailIcon::Settings,
                                    palette,
                                );
                                if settings.clicked() {
                                    state.open_settings();
                                }
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &settings,
                                    "Settings (Cmd/Ctrl+,)",
                                    state.rail_expanded,
                                );

                                ui.add_space(4.0);
                                let downloads = Self::rail_icon_button(
                                    ui,
                                    vec2(34.0, 30.0),
                                    RailIcon::Download,
                                    palette,
                                );
                                if downloads.clicked() {
                                    actions.push(UiAction::OpenDownloads);
                                }
                                paint_download_badge(
                                    ui,
                                    downloads.rect,
                                    download_badge,
                                    palette,
                                    true,
                                );
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &downloads,
                                    "Downloads (Cmd/Ctrl+J)",
                                    state.rail_expanded,
                                );

                                ui.add_space(4.0);
                                let icon_size = rail_status_icon_size(false);
                                let badge_width = width - 10.0;
                                let badge_height = RAIL_STATUS_BADGE_HEIGHT;

                                let (blocked_rect, blocked_response) = ui.allocate_exact_size(
                                    vec2(badge_width, badge_height),
                                    Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    blocked_rect,
                                    Rounding::same(7.0),
                                    palette.rail_surface,
                                );
                                paint_rail_icon(
                                    ui,
                                    egui::Rect::from_center_size(
                                        blocked_rect.center(),
                                        vec2(icon_size, icon_size),
                                    ),
                                    RailIcon::Shield,
                                    palette.shield_ok,
                                );
                                let blocked_requests = state
                                    .tabs
                                    .iter()
                                    .find(|tab| Some(tab.id) == state.active_tab_id)
                                    .map(|tab| tab.blocked_requests)
                                    .unwrap_or(0);
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &blocked_response,
                                    format!("{blocked_requests} ads blocked"),
                                    false,
                                );

                                ui.add_space(3.0);
                                let (ram_rect, ram_response) = ui.allocate_exact_size(
                                    vec2(badge_width, badge_height),
                                    Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    ram_rect,
                                    Rounding::same(7.0),
                                    palette.rail_surface,
                                );
                                paint_rail_icon(
                                    ui,
                                    egui::Rect::from_center_size(
                                        ram_rect.center(),
                                        vec2(icon_size, icon_size),
                                    ),
                                    RailIcon::Chip,
                                    palette.text,
                                );
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &ram_response,
                                    format!("RAM {} MB", state.ram_mb),
                                    false,
                                );
                            });
                        });
                    });
                    return;
                }

                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    let rail_rect = ui.clip_rect();
                    ui.add_space(7.0);
                    ui.add_space(3.0);
                    let command = Self::rail_action_button(
                        ui,
                        width - 12.0,
                        32.0,
                        RailIcon::Search,
                        "Command",
                        palette,
                    );
                    if command.clicked() {
                        state.open_command_palette();
                    }
                    Self::chrome_tooltip(
                        tooltip_slot,
                        ui,
                        &command,
                        "Command palette (Cmd/Ctrl+K)",
                        state.rail_expanded,
                    );

                    ui.add_space(3.0);
                    let new_tab = Self::rail_action_button(
                        ui,
                        width - 12.0,
                        32.0,
                        RailIcon::Plus,
                        "New tab",
                        palette,
                    );
                    if new_tab.clicked() {
                        actions.push(UiAction::NewTab { url: None });
                    }
                    Self::chrome_tooltip(
                        tooltip_slot,
                        ui,
                        &new_tab,
                        "New tab (Cmd/Ctrl+T)",
                        state.rail_expanded,
                    );

                    ui.add_space(3.0);
                    let devtools = Self::rail_action_button(
                        ui,
                        width - 12.0,
                        32.0,
                        RailIcon::Code,
                        "DevTools",
                        palette,
                    );
                    if devtools.clicked() {
                        if let Some(tab_id) = state.active_tab_id {
                            actions.push(UiAction::OpenDevTools { tab_id });
                        }
                    }
                    Self::chrome_tooltip(
                        tooltip_slot,
                        ui,
                        &devtools,
                        "DevTools (F12)",
                        state.rail_expanded,
                    );

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // Framed omnibox: the helper reserves the fixed glyph slots
                    // and uses a zero-margin TextEdit so the frame stays inside
                    // the rail with even side margins.
                    Self::build_rail_address_field(ui, ctx, state, actions, &palette);
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);

                    ui.add_space(4.0);
                    Self::build_workspace_switcher(
                        ui,
                        state,
                        actions,
                        tooltip_slot,
                        &palette,
                        width,
                        true,
                    );
                    ui.add_space(6.0);

                    if state
                        .tabs
                        .iter()
                        .any(|tab| tab.workspace_id == state.active_workspace_id && tab.is_pinned)
                    {
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new("PINNED")
                                    .size(9.0)
                                    .color(palette.section_label),
                            );
                        });
                        ui.add_space(1.0);
                    }

                    egui::ScrollArea::vertical()
                        .id_salt("rail_tabs")
                        .show(ui, |ui| {
                            for tab in state.tabs.iter().filter(|tab| {
                                tab.workspace_id == state.active_workspace_id && tab.is_pinned
                            }) {
                                self.rail_tab_row(
                                    ui,
                                    tab,
                                    state,
                                    actions,
                                    tooltip_slot,
                                    &mut menu_request,
                                    rail_rect,
                                    palette,
                                    width,
                                    true,
                                );
                            }
                            if state.tabs.iter().any(|tab| {
                                tab.workspace_id == state.active_workspace_id && tab.is_pinned
                            }) && state.tabs.iter().any(|tab| {
                                tab.workspace_id == state.active_workspace_id && !tab.is_pinned
                            }) {
                                ui.add_space(3.0);
                                ui.separator();
                                ui.add_space(3.0);
                            }
                            for tab in state.tabs.iter().filter(|tab| {
                                tab.workspace_id == state.active_workspace_id && !tab.is_pinned
                            }) {
                                self.rail_tab_row(
                                    ui,
                                    tab,
                                    state,
                                    actions,
                                    tooltip_slot,
                                    &mut menu_request,
                                    rail_rect,
                                    palette,
                                    width,
                                    true,
                                );
                            }
                        });
                });

                // Status block stays at the bottom of the rail.
                let status_rect = egui::Rect::from_min_max(
                    pos2(0.0, ui.clip_rect().bottom() - RAIL_STATUS_HEIGHT),
                    pos2(width, ui.clip_rect().bottom()),
                );
                ui.allocate_new_ui(UiBuilder::new().max_rect(status_rect), |ui| {
                    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                        ui.add_space(6.0);
                        let collapse = Self::rail_action_button(
                            ui,
                            width - 12.0,
                            30.0,
                            RailIcon::Sidebar,
                            "Collapse rail",
                            palette,
                        );
                        if collapse.clicked() {
                            state.toggle_rail(actions);
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &collapse,
                            "Collapse workspace rail",
                            state.rail_expanded,
                        );

                        ui.add_space(4.0);
                        let hibernate = Self::rail_action_button(
                            ui,
                            width - 12.0,
                            30.0,
                            RailIcon::Moon,
                            "Hibernate",
                            palette,
                        );
                        if hibernate.clicked() {
                            actions.push(UiAction::HibernateAllBackground);
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &hibernate,
                            "Hibernate tabs (Alt+H)",
                            state.rail_expanded,
                        );

                        ui.add_space(4.0);
                        let settings = Self::rail_action_button(
                            ui,
                            width - 12.0,
                            30.0,
                            RailIcon::Settings,
                            "Settings",
                            palette,
                        );
                        if settings.clicked() {
                            state.open_settings();
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &settings,
                            "Settings (Cmd/Ctrl+,)",
                            state.rail_expanded,
                        );

                        ui.add_space(4.0);
                        let downloads = Self::rail_action_button(
                            ui,
                            width - 12.0,
                            30.0,
                            RailIcon::Download,
                            "Downloads",
                            palette,
                        );
                        if downloads.clicked() {
                            actions.push(UiAction::OpenDownloads);
                        }
                        paint_download_badge(ui, downloads.rect, download_badge, palette, false);
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &downloads,
                            "Downloads (Cmd/Ctrl+J)",
                            state.rail_expanded,
                        );

                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let icon_size = rail_status_icon_size(true);
                            let (icon_rect, _) =
                                ui.allocate_exact_size(vec2(icon_size, icon_size), Sense::hover());
                            paint_rail_icon(ui, icon_rect, RailIcon::Shield, palette.text_dim);
                            ui.add_space(3.0);
                            let blocked = state
                                .tabs
                                .iter()
                                .find(|t| Some(t.id) == state.active_tab_id)
                                .map(|t| t.blocked_requests)
                                .unwrap_or(0);
                            ui.label(
                                RichText::new(format!("{blocked}"))
                                    .size(11.0)
                                    .color(palette.shield_ok),
                            );
                        });

                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let icon_size = rail_status_icon_size(true);
                            let (icon_rect, _) =
                                ui.allocate_exact_size(vec2(icon_size, icon_size), Sense::hover());
                            paint_rail_icon(ui, icon_rect, RailIcon::Chip, palette.text_dim);
                            ui.add_space(3.0);
                            ui.label(
                                RichText::new(format!("RAM {} MB", state.ram_mb))
                                    .size(11.0)
                                    .color(palette.text_dim),
                            );
                        });
                    });
                });
            });
        // Applied after the panel closure releases its borrow of `state`.
        if let Some((tab_id, anchor)) = menu_request {
            state.open_tab_menu(tab_id, anchor);
        }
        state.tooltip = if state.menu.is_some() {
            None
        } else {
            rail_tooltip
        };

        // Keep the rail width associated with the panel for tests/debugging.
        let _ = rail.response.rect;
    }

    /// Draw the floating command palette over a dimmed full-window layer.
    pub fn build_command_palette(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        focus_command: bool,
    ) {
        let items = command_palette_items(
            &state.command_palette_query,
            &state.tabs,
            state.active_tab_id,
            &state.preferences,
        );
        state.command_selection = clamp_command_selection(state.command_selection, items.len());

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.close_command_palette();
            return;
        }
        let move_up = ctx.input(|i| i.key_pressed(egui::Key::ArrowUp));
        let move_down = ctx.input(|i| i.key_pressed(egui::Key::ArrowDown));
        state.command_selection =
            advance_command_selection(state.command_selection, items.len(), move_up, move_down);

        let execute_enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let mut executed = false;
        let palette = state.palette();
        let palette = &palette;

        let screen = ctx.screen_rect();
        let palette_width = COMMAND_PALETTE_WIDTH.min((screen.width() - 24.0).max(200.0));
        let palette_x = (screen.width() - palette_width) / 2.0;
        let palette_y = COMMAND_PALETTE_TOP_OFFSET.min((screen.height() - 120.0).max(4.0));

        egui::Area::new(egui::Id::new("command_overlay"))
            .order(Order::Foreground)
            .fixed_pos(screen.min)
            .movable(false)
            .show(ctx, |ui| {
                // Allocate the full screen so the Area's clip rect includes the
                // overlay; painting outside an unallocated Area gets clipped.
                let (overlay_rect, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
                ui.painter()
                    .rect_filled(overlay_rect, Rounding::ZERO, palette.scrim);
            });

        let mut palette_rect =
            egui::Rect::from_min_size(pos2(palette_x, palette_y), vec2(palette_width, 0.0));
        egui::Area::new(egui::Id::new("command_palette"))
            .order(Order::Foreground)
            .fixed_pos(pos2(palette_x, palette_y))
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(palette_width);
                palette_rect = Frame::none()
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .rounding(Rounding::same(12.0))
                    .inner_margin(egui::Margin::symmetric(14.0, 12.0))
                    .show(ui, |ui| {
                        let input_id = egui::Id::new("kasumi_command_palette_input");
                        if focus_command {
                            ctx.memory_mut(|mem| mem.request_focus(input_id));
                        }
                        let input = ui.add(
                            TextEdit::singleline(&mut state.command_palette_query)
                                .id(input_id)
                                .hint_text("Search, open a tab, or enter an address…")
                                .text_color(palette.text_strong)
                                .frame(false)
                                .desired_width(palette_width - 28.0),
                        );
                        if focus_command {
                            input.request_focus();
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(6.0);

                        if items.is_empty() {
                            ui.label(
                                RichText::new("No matching commands")
                                    .color(palette.palette_subtext)
                                    .size(12.0),
                            );
                        } else {
                            for (index, item) in items.iter().enumerate() {
                                let selected = index == state.command_selection;
                                let (row_rect, row_resp) = ui.allocate_exact_size(
                                    vec2(palette_width - 28.0, 34.0),
                                    Sense::click(),
                                );
                                let fill = palette_row_fill(palette, selected, row_resp.hovered());
                                ui.painter()
                                    .rect_filled(row_rect, Rounding::same(6.0), fill);

                                // Lay the labels out *inside* the allocated row.
                                // Adding them after `allocate_exact_size` would
                                // place them below the highlight rect.
                                ui.allocate_new_ui(UiBuilder::new().max_rect(row_rect), |ui| {
                                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                        ui.add_space(8.0);
                                        ui.label(RichText::new(item.icon).size(14.0));
                                        ui.add_space(4.0);
                                        ui.label(
                                            RichText::new(&item.label)
                                                .size(12.0)
                                                .color(palette.text_strong),
                                        );
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(truncate_title(&item.subtitle, 38))
                                                .size(10.0)
                                                .color(palette.palette_subtext),
                                        );
                                    });
                                });

                                if row_resp.clicked() {
                                    apply_command_item(&item.action, state, actions);
                                    executed = true;
                                }
                            }
                        }

                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("↑↓ select   ↵ run   esc close")
                                    .size(10.0)
                                    .color(palette.palette_hint),
                            );
                        });
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        // A primary click anywhere outside the palette card closes it. The
        // dim layer is part of the same transparent overlay window, so the
        // page behind stays visible while this happens.
        let outside_click = ctx.input(|i| {
            command_palette_click_outside(
                palette_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        });
        if outside_click {
            state.close_command_palette();
            return;
        }

        if execute_enter && !executed {
            if let Some(item) = items.get(state.command_selection) {
                apply_command_item(&item.action, state, actions);
            }
        }
    }

    /// Draw the right-click tab menu in the shared transparent overlay.
    ///
    /// The native CEF child view sits above the main window's wgpu layer, so
    /// this menu must live in a separate top-level window. It is intentionally
    /// not dimmed: the page remains visible behind the card, and a primary
    /// click anywhere outside the card dismisses it.
    fn build_tab_menu(ctx: &egui::Context, state: &mut ChromeState, actions: &mut Vec<UiAction>) {
        let Some(menu) = state.menu.clone() else {
            return;
        };
        let items = rail_menu_items(state, menu.tab_id);
        state.menu_selection = clamp_command_selection(state.menu_selection, items.len());

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.close_tab_menu();
            return;
        }
        let move_up = ctx.input(|i| i.key_pressed(egui::Key::ArrowUp));
        let move_down = ctx.input(|i| i.key_pressed(egui::Key::ArrowDown));
        state.menu_selection =
            advance_command_selection(state.menu_selection, items.len(), move_up, move_down);
        let execute_enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let mut executed = false;
        let palette = state.palette();
        let palette = &palette;

        let screen = ctx.screen_rect();
        let width = menu.width.min((screen.width() - 12.0).max(160.0));
        let height = menu.height.min((screen.height() - 12.0).max(96.0));
        let x = menu.anchor.x.clamp(
            screen.left() + 4.0,
            (screen.right() - width - 4.0).max(screen.left() + 4.0),
        );
        let y = menu.anchor.y.clamp(
            screen.top() + 4.0,
            (screen.bottom() - height - 4.0).max(screen.top() + 4.0),
        );
        let anchor = pos2(x, y);
        let mut card_rect = egui::Rect::from_min_size(anchor, vec2(width, height));

        egui::Area::new(egui::Id::new("tab_context_menu"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width - 4.0);
                card_rect = Frame::none()
                    .fill(palette.menu_bg)
                    .stroke(Stroke::new(1.0_f32, palette.menu_border))
                    .rounding(Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(6.0, 6.0))
                    .show(ui, |ui| {
                        for (index, item) in items.iter().enumerate() {
                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                vec2(width - 16.0, RAIL_MENU_ROW_HEIGHT),
                                Sense::click(),
                            );
                            let selected = index == state.menu_selection;
                            let fill = palette_row_fill(
                                palette,
                                selected,
                                row_resp.hovered() && item.enabled,
                            );
                            ui.painter()
                                .rect_filled(row_rect, Rounding::same(5.0), fill);

                            let text_color = if !item.enabled {
                                palette.menu_disabled
                            } else if selected {
                                palette.text_strong
                            } else {
                                palette.text
                            };
                            // Paint directly into the same rect that receives
                            // the click. A nested `allocate_new_ui` also advances
                            // the parent cursor, which drifts the painted rows
                            // away from their hover targets.
                            let label = item.label.to_string();
                            ui.painter().text(
                                pos2(row_rect.min.x + 10.0, row_rect.center().y),
                                Align2::LEFT_CENTER,
                                label.as_str(),
                                FontId::new(12.0, FontFamily::Proportional),
                                text_color,
                            );

                            if row_resp.clicked() && item.enabled {
                                apply_menu_item(ctx, &item.action, state, actions);
                                executed = true;
                            }
                        }
                    })
                    .response
                    .rect
                    .expand(2.0);
            });

        let outside_click = ctx.input(|i| {
            rail_menu_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        });
        if outside_click {
            state.close_tab_menu();
            return;
        }

        if execute_enter && !executed {
            if let Some(item) = items.get(state.menu_selection) {
                if item.enabled {
                    apply_menu_item(ctx, &item.action, state, actions);
                }
            }
        }
    }

    /// Draw the Arc/Edge-style downloads bubble in the shared overlay.
    ///
    /// The bubble is anchored beside the rail near its Downloads button. It is
    /// intentionally not dimmed: the page stays visible behind it, and a click
    /// outside the card dismisses it. Every row reports individual progress and
    /// exposes the same native download actions as the former Settings page.
    pub fn build_downloads_bubble(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        if !state.downloads_open {
            return;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.close_downloads();
            return;
        }

        let downloads = state.downloads.clone();
        let summary = DownloadSummary::from_downloads(&downloads);
        let has_finished = downloads
            .iter()
            .any(|download| download.is_finished() || download.state == DownloadState::Canceled);
        let palette = state.palette();
        let palette = &palette;
        let screen = ctx.screen_rect();
        let size = downloads_bubble_size(screen);
        let anchor = downloads_bubble_anchor(screen, rail_width(state.rail_expanded));
        let mut card_rect = egui::Rect::from_min_size(anchor, size);

        egui::Area::new(egui::Id::new("kasumi_downloads_bubble"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(size.x);
                card_rect = Frame::none()
                    .fill(palette.menu_bg)
                    .stroke(Stroke::new(1.0_f32, palette.menu_border))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Downloads")
                                    .size(14.0)
                                    .strong()
                                    .color(palette.text_strong),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if material_button(
                                    ui,
                                    RailIcon::Close,
                                    "",
                                    MaterialButtonStyle::Text,
                                    palette,
                                    true,
                                )
                                .clicked()
                                {
                                    state.close_downloads();
                                }
                                if material_button(
                                    ui,
                                    RailIcon::Delete,
                                    "Clear finished",
                                    MaterialButtonStyle::Text,
                                    palette,
                                    has_finished,
                                )
                                .clicked()
                                {
                                    actions.push(UiAction::ClearCompletedDownloads);
                                }
                            });
                        });
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(summary.status_text()).size(11.0).color(
                                if summary.failed > 0 {
                                    palette.close_icon
                                } else {
                                    palette.rail_accent
                                },
                            ));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let mut parts = vec![format!("{}%", summary.percent())];
                                if summary.speed > 0 {
                                    parts.push(format!(
                                        "{} /s",
                                        format_download_bytes(summary.speed)
                                    ));
                                }
                                let eta = download_eta_label(
                                    summary.received,
                                    summary.total,
                                    summary.speed,
                                );
                                if !eta.is_empty() {
                                    parts.push(eta);
                                }
                                ui.label(
                                    RichText::new(parts.join("  ·  "))
                                        .size(10.0)
                                        .color(palette.palette_hint),
                                );
                            });
                        });
                        // Use a finite width: an infinite desired width would be
                        // cached as the Area size and poison the next frame with
                        // a NaN centering rect.
                        ui.add(
                            egui::ProgressBar::new(summary.percent() as f32 / 100.0)
                                .text(format!("{}%", summary.percent()))
                                .desired_width((size.x - 20.0).max(40.0)),
                        );
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(5.0);

                        let rows_height = (size.y - 118.0).max(70.0);
                        egui::ScrollArea::vertical()
                            .id_salt("kasumi_downloads_bubble_rows")
                            .max_height(rows_height)
                            .show(ui, |ui| {
                                if downloads.is_empty() {
                                    ui.add_space(18.0);
                                    ui.label(
                                        RichText::new("No downloads yet")
                                            .size(12.0)
                                            .color(palette.text_dim),
                                    );
                                    return;
                                }

                                for download in &downloads {
                                    ui.add_space(5.0);
                                    ui.separator();
                                    ui.add_space(5.0);
                                    let name = if download.suggested_name.is_empty() {
                                        download.url.as_str()
                                    } else {
                                        download.suggested_name.as_str()
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(truncate_title(name, 34))
                                                .size(11.0)
                                                .strong(),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(download_state_label(
                                                        download.state,
                                                    ))
                                                    .size(10.0)
                                                    .color(match download.state {
                                                        DownloadState::Completed => {
                                                            palette.shield_ok
                                                        }
                                                        DownloadState::Canceled
                                                        | DownloadState::Interrupted => {
                                                            palette.close_icon
                                                        }
                                                        _ => palette.rail_accent,
                                                    }),
                                                );
                                            },
                                        );
                                    });

                                    // Active and paused transfers keep a progress
                                    // bar with compact pause/resume and cancel
                                    // icons on its right edge. Finished rows drop
                                    // the bar entirely and swap those icons for
                                    // open/show-in-folder so each row stays short.
                                    if download_row_uses_progress_bar(download.state) {
                                        let percent =
                                            download.percent_complete.clamp(0, 100) as f32;
                                        ui.horizontal(|ui| {
                                            ui.add(
                                                egui::ProgressBar::new(percent / 100.0)
                                                    .text(format!("{percent:.0}%"))
                                                    .desired_width((size.x - 68.0).max(40.0)),
                                            );
                                            match download.state {
                                                DownloadState::InProgress => {
                                                    if compact_icon_button(
                                                        ui,
                                                        RailIcon::Pause,
                                                        "Pause",
                                                        palette,
                                                        true,
                                                    )
                                                    .clicked()
                                                    {
                                                        actions.push(UiAction::PauseDownload {
                                                            download_id: download.id,
                                                        });
                                                    }
                                                    if compact_icon_button(
                                                        ui,
                                                        RailIcon::Close,
                                                        "Cancel",
                                                        palette,
                                                        false,
                                                    )
                                                    .clicked()
                                                    {
                                                        actions.push(UiAction::CancelDownload {
                                                            download_id: download.id,
                                                        });
                                                    }
                                                }
                                                DownloadState::Paused => {
                                                    if compact_icon_button(
                                                        ui,
                                                        RailIcon::Play,
                                                        "Resume",
                                                        palette,
                                                        true,
                                                    )
                                                    .clicked()
                                                    {
                                                        actions.push(UiAction::ResumeDownload {
                                                            download_id: download.id,
                                                        });
                                                    }
                                                    if compact_icon_button(
                                                        ui,
                                                        RailIcon::Close,
                                                        "Cancel",
                                                        palette,
                                                        false,
                                                    )
                                                    .clicked()
                                                    {
                                                        actions.push(UiAction::CancelDownload {
                                                            download_id: download.id,
                                                        });
                                                    }
                                                }
                                                _ => {}
                                            }
                                        });
                                    } else {
                                        ui.horizontal(|ui| match download.state {
                                            DownloadState::Completed => {
                                                if compact_icon_button(
                                                    ui,
                                                    RailIcon::OpenInNew,
                                                    "Open file",
                                                    palette,
                                                    true,
                                                )
                                                .clicked()
                                                {
                                                    actions.push(UiAction::OpenDownloadFile {
                                                        download_id: download.id,
                                                    });
                                                }
                                                if compact_icon_button(
                                                    ui,
                                                    RailIcon::FolderOpen,
                                                    "Show in folder",
                                                    palette,
                                                    false,
                                                )
                                                .clicked()
                                                {
                                                    actions.push(UiAction::RevealDownload {
                                                        download_id: download.id,
                                                    });
                                                }
                                            }
                                            DownloadState::Canceled
                                            | DownloadState::Interrupted => {
                                                if compact_icon_button(
                                                    ui,
                                                    RailIcon::Retry,
                                                    "Retry",
                                                    palette,
                                                    true,
                                                )
                                                .clicked()
                                                {
                                                    actions.push(UiAction::RetryDownload {
                                                        download_id: download.id,
                                                    });
                                                }
                                            }
                                            _ => {}
                                        });
                                    }
                                    let eta = if download.state == DownloadState::InProgress {
                                        download_eta_label(
                                            download.received_bytes,
                                            download.total_bytes,
                                            download.current_speed,
                                        )
                                    } else {
                                        String::new()
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} / {}",
                                                format_download_bytes(download.received_bytes),
                                                format_download_bytes(download.total_bytes)
                                            ))
                                            .size(11.0)
                                            .strong()
                                            .color(palette.text_strong),
                                        );
                                        if download.current_speed > 0 {
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} /s",
                                                    format_download_bytes(download.current_speed)
                                                ))
                                                .size(11.0)
                                                .color(palette.text),
                                            );
                                        }
                                        if !eta.is_empty() {
                                            ui.label(
                                                RichText::new(eta)
                                                    .size(11.0)
                                                    .color(palette.palette_hint),
                                            );
                                        }
                                        ui.label(
                                            RichText::new(download_engine_label(
                                                download.engine,
                                                download.segments,
                                            ))
                                            .size(11.0)
                                            .color(palette.text),
                                        );
                                        // "Remove info" only forgets the SQLite
                                        // row and UI state; the file on disk is
                                        // never deleted by this action.
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if compact_icon_button(
                                                    ui,
                                                    RailIcon::Delete,
                                                    "Remove info",
                                                    palette,
                                                    false,
                                                )
                                                .clicked()
                                                {
                                                    actions.push(UiAction::RemoveDownload {
                                                        download_id: download.id,
                                                    });
                                                }
                                            },
                                        );
                                    });
                                    if !download.full_path.is_empty() {
                                        ui.label(
                                            RichText::new(truncate_title(&download.full_path, 48))
                                                .size(10.0)
                                                .color(palette.text),
                                        );
                                    }
                                }
                            });
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        let outside_click = ctx.input(|i| {
            command_palette_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        });
        if outside_click {
            state.close_downloads();
        }
    }

    /// Draw the native Settings card in the shared transparent overlay.
    ///
    /// Controls commit immediately to `ChromeState` and emit one
    /// `UiAction::ApplyPreferences`; the winit handler persists the document and
    /// updates the CEF-backed runtime. No CEF thread is touched here.
    pub fn build_settings(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        let mut draft = state.settings_draft.clone();
        let mut edit_name = state.workspace_edit_name_draft.clone();
        let mut edit_icon = state.workspace_edit_icon_draft;
        let mut edit_color = state.workspace_edit_color_draft;
        let mut new_name = state.workspace_new_name_draft.clone();
        let mut new_icon = state.workspace_new_icon_draft;
        let mut new_color = state.workspace_new_color_draft;
        let workspaces = state.workspaces.clone();
        let active_workspace = state.active_workspace().cloned();
        let workspace_count = state.workspaces.len();
        let mut settings_tab = state.settings_tab;
        let active_tab_id = state.active_tab_id;
        let bookmarks = state.bookmarks.clone();
        let mut workspace_actions: Vec<UiAction> = Vec::new();
        let mut selected_workspace_id = state
            .selected_workspace_id
            .or_else(|| active_workspace.as_ref().map(|workspace| workspace.id))
            .or_else(|| workspaces.first().map(|workspace| workspace.id));
        if !workspaces
            .iter()
            .any(|workspace| Some(workspace.id) == selected_workspace_id)
        {
            if let Some(workspace) = active_workspace.as_ref().or_else(|| workspaces.first()) {
                selected_workspace_id = Some(workspace.id);
                edit_name = workspace.name.clone();
                edit_icon = workspace.icon;
                edit_color = workspace.color;
            }
        }
        let selected_workspace = selected_workspace_id.and_then(|id| {
            workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .cloned()
        });
        let workspace_create_mode = state.workspace_create_mode;
        let mut save_clicked = false;
        let mut create_clicked = false;
        let mut cancel_create_clicked = false;
        let mut new_workspace_clicked = false;
        let mut delete_clicked = false;
        let mut make_default_clicked = false;
        let mut switch_clicked = false;
        let palette = state.palette();
        let palette = &palette;
        let screen = ctx.screen_rect();
        let width = SETTINGS_WIDTH.min((screen.width() - 24.0).max(280.0));
        let mut card_rect = egui::Rect::from_min_size(screen.center(), vec2(width, 0.0));
        let mut done_clicked = false;
        let requested_close = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        // Center the card inside the overlay, which is aligned to the main
        // window. This makes Settings behave like a dialog on whichever display
        // currently contains the browser, rather than near the primary screen.
        egui::Area::new(egui::Id::new("kasumi_settings"))
            .order(Order::Foreground)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .fixed_pos(screen.center())
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::none()
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .rounding(Rounding::same(12.0))
                    .inner_margin(egui::Margin::symmetric(20.0, 18.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Settings")
                                    .size(16.0)
                                    .strong()
                                    .color(palette.text_strong),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                done_clicked = material_button(
                                    ui,
                                    RailIcon::Check,
                                    "Done",
                                    MaterialButtonStyle::Tonal,
                                    palette,
                                    true,
                                )
                                .clicked();
                            });
                        });
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            for (tab, label) in [
                                (SettingsTab::General, "General"),
                                (SettingsTab::Workspaces, "Workspaces"),
                                (SettingsTab::Bookmarks, "Bookmarks"),
                            ] {
                                if ui.selectable_label(settings_tab == tab, label).clicked() {
                                    settings_tab = tab;
                                }
                            }
                        });
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(6.0);

                        let available_height =
                            SETTINGS_MAX_CONTENT_HEIGHT.min((screen.height() - 120.0).max(160.0));
                        let scroll_id = match settings_tab {
                            SettingsTab::General => "kasumi_settings_general",
                            SettingsTab::Workspaces => "kasumi_settings_workspaces",
                            SettingsTab::Bookmarks => "kasumi_settings_bookmarks",
                        };
                        egui::ScrollArea::vertical()
                            .id_salt(scroll_id)
                            .max_height(available_height)
                            .show(ui, |ui| {
                                match settings_tab {
                                    SettingsTab::General => {
                                ui.label(
                                    RichText::new("APPEARANCE")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                ui.radio_value(
                                    &mut draft.theme,
                                    ThemePreference::System,
                                    "Follow system appearance",
                                );
                                ui.radio_value(&mut draft.theme, ThemePreference::Light, "Light");
                                ui.radio_value(&mut draft.theme, ThemePreference::Dark, "Dark");

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("SEARCH")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                ui.horizontal_wrapped(|ui| {
                                    for engine in SearchEngine::ALL {
                                        ui.radio_value(
                                            &mut draft.search_engine,
                                            engine,
                                            engine.label(),
                                        );
                                    }
                                });
                                ui.label(
                                    RichText::new("Used for omnibox text that is not a URL.")
                                        .size(10.0)
                                        .color(palette.palette_hint),
                                );

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("PRIVACY")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                ui.checkbox(
                                    &mut draft.adblock_enabled,
                                    "Block ads and trackers before page scripts load",
                                );

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("PERFORMANCE")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                ui.horizontal(|ui| {
                                    ui.label("Hibernate idle background tabs after");
                                    ui.add(
                                        egui::Slider::new(
                                            &mut draft.idle_timeout_minutes,
                                            MIN_IDLE_TIMEOUT_MINUTES..=MAX_IDLE_TIMEOUT_MINUTES,
                                        )
                                        .suffix(" min"),
                                    );
                                });
                                ui.label(
                                    RichText::new(
                                        "Critical RAM pressure still hibernates immediately.",
                                    )
                                    .size(10.0)
                                    .color(palette.palette_hint),
                                );

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("STARTUP")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                ui.label("Open on launch:");
                                ui.horizontal(|ui| {
                                    for behavior in WorkspaceStartupBehavior::ALL {
                                        ui.radio_value(
                                            &mut draft.workspace_startup,
                                            behavior,
                                            behavior.label(),
                                        );
                                    }
                                });
                                ui.label(
                                    RichText::new(
                                        "Last used space restores where you left off; Default space always opens the space selected in Workspaces.",
                                    )
                                    .size(10.0)
                                    .color(palette.palette_hint),
                                );
                                ui.add_space(4.0);
                                ui.checkbox(
                                    &mut draft.rail_expanded,
                                    "Expand the workspace rail on startup",
                                );

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("DOWNLOADS")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                ui.checkbox(
                                    &mut draft.ask_where_to_save,
                                    "Ask where to save each file",
                                );
                                ui.add_space(3.0);
                                ui.add_enabled(
                                    !draft.ask_where_to_save,
                                    egui::Label::new("Download folder (leave empty for the default Downloads folder):"),
                                );
                                ui.add_enabled(
                                    !draft.ask_where_to_save,
                                    egui::TextEdit::singleline(&mut draft.download_directory)
                                        .desired_width(320.0)
                                        .hint_text("~/Downloads"),
                                );
                                ui.label(
                                    RichText::new(if draft.ask_where_to_save {
                                        "The save dialog chooses the destination while this is enabled."
                                    } else {
                                        "Files are saved automatically to the folder above."
                                    })
                                    .size(10.0)
                                    .color(palette.palette_hint),
                                );
                                }
                                SettingsTab::Workspaces => {
                                    if workspace_create_mode {
                                        ui.label(
                                            RichText::new("CREATE A WORKSPACE")
                                                .size(10.0)
                                                .strong()
                                                .color(palette.section_label),
                                        );
                                        ui.label(
                                            RichText::new(
                                                "Choose a name, icon, and accent. Your current workspace stays untouched until you create it.",
                                            )
                                            .size(10.0)
                                            .color(palette.palette_hint),
                                        );
                                        ui.add_space(6.0);
                                        ui.horizontal(|ui| {
                                            ui.label("Name");
                                            ui.add(
                                                TextEdit::singleline(&mut new_name)
                                                    .desired_width(240.0),
                                            );
                                        });
                                        ui.add_space(5.0);
                                        ui.label(
                                            RichText::new("ICON")
                                                .size(9.0)
                                                .color(palette.section_label),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            for icon in WorkspaceIcon::ALL {
                                                if ui
                                                    .selectable_label(new_icon == icon, icon.label())
                                                    .clicked()
                                                {
                                                    new_icon = icon;
                                                }
                                            }
                                        });
                                        ui.add_space(5.0);
                                        ui.label(
                                            RichText::new("ACCENT")
                                                .size(9.0)
                                                .color(palette.section_label),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            for color in WorkspaceColor::ALL {
                                                let (r, g, b) = color.rgb();
                                                let swatch =
                                                    RichText::new(format!("● {}", color.label()))
                                                        .color(Color32::from_rgb(r, g, b));
                                                if ui
                                                    .selectable_label(new_color == color, swatch)
                                                    .clicked()
                                                {
                                                    new_color = color;
                                                }
                                            }
                                        });
                                        ui.add_space(8.0);
                                        ui.horizontal(|ui| {
                                            create_clicked = material_button(
                                                ui,
                                                RailIcon::Plus,
                                                "Create workspace",
                                                MaterialButtonStyle::Filled,
                                                palette,
                                                true,
                                            )
                                            .clicked();
                                            cancel_create_clicked = material_button(
                                                ui,
                                                RailIcon::ChevronLeft,
                                                "Back",
                                                MaterialButtonStyle::Outlined,
                                                palette,
                                                true,
                                            )
                                            .clicked();
                                        });
                                    } else {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("WORKSPACES")
                                                    .size(10.0)
                                                    .strong()
                                                    .color(palette.section_label),
                                            );
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    new_workspace_clicked = material_button(
                                                        ui,
                                                        RailIcon::Plus,
                                                        "New workspace",
                                                        MaterialButtonStyle::Tonal,
                                                        palette,
                                                        true,
                                                    )
                                                    .clicked();
                                                },
                                            );
                                        });
                                        ui.label(
                                            RichText::new(
                                                "Pick a workspace to edit. Creation happens in its own form.",
                                            )
                                            .size(10.0)
                                            .color(palette.palette_hint),
                                        );
                                        ui.add_space(6.0);

                                        ui.label(
                                            RichText::new("YOUR WORKSPACES")
                                                .size(9.0)
                                                .color(palette.section_label),
                                        );
                                        for workspace in &workspaces {
                                            let is_selected = Some(workspace.id) == selected_workspace_id;
                                            let width = ui.available_width();
                                            let (rect, response) =
                                                ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
                                            let fill = if is_selected {
                                                palette.rail_row_active
                                            } else if response.hovered() {
                                                palette.rail_row_hover
                                            } else {
                                                Color32::TRANSPARENT
                                            };
                                            ui.painter().rect_filled(rect, Rounding::same(8.0), fill);
                                            let (r, g, b) = workspace.color.rgb();
                                            paint_rail_icon(
                                                ui,
                                                egui::Rect::from_center_size(
                                                    pos2(rect.min.x + 18.0, rect.center().y),
                                                    vec2(18.0, 18.0),
                                                ),
                                                workspace.icon.into(),
                                                Color32::from_rgb(r, g, b),
                                            );
                                            let mut badges = Vec::new();
                                            if workspace.is_default {
                                                badges.push("Default");
                                            }
                                            if workspace.is_active {
                                                badges.push("Active");
                                            }
                                            let suffix = if badges.is_empty() {
                                                String::new()
                                            } else {
                                                format!("  ·  {}", badges.join(" · "))
                                            };
                                            ui.painter().text(
                                                pos2(rect.min.x + 36.0, rect.center().y),
                                                Align2::LEFT_CENTER,
                                                format!(
                                                    "{}{}",
                                                    truncate_title(&workspace.name, 24),
                                                    suffix
                                                ),
                                                FontId::new(11.0, FontFamily::Proportional),
                                                if is_selected {
                                                    palette.text_strong
                                                } else {
                                                    palette.text
                                                },
                                            );
                                            if response.clicked() {
                                                selected_workspace_id = Some(workspace.id);
                                                edit_name = workspace.name.clone();
                                                edit_icon = workspace.icon;
                                                edit_color = workspace.color;
                                            }
                                        }

                                        ui.add_space(8.0);
                                        ui.separator();
                                        ui.add_space(6.0);
                                        if let Some(workspace) = &selected_workspace {
                                            ui.label(
                                                RichText::new(format!("EDIT “{}”", workspace.name))
                                                    .size(10.0)
                                                    .strong()
                                                    .color(palette.palette_subtext),
                                            );
                                            ui.add_space(3.0);
                                            ui.horizontal(|ui| {
                                                if workspace.is_default {
                                                    ui.label(
                                                        RichText::new("DEFAULT WORKSPACE")
                                                            .size(9.0)
                                                            .color(palette.rail_accent),
                                                    );
                                                } else {
                                                    make_default_clicked = material_button(
                                                        ui,
                                                        RailIcon::Star,
                                                        "Make default",
                                                        MaterialButtonStyle::Tonal,
                                                        palette,
                                                        true,
                                                    )
                                                    .clicked();
                                                }
                                                if !workspace.is_active {
                                                    switch_clicked = material_button(
                                                        ui,
                                                        RailIcon::OpenInNew,
                                                        "Switch to",
                                                        MaterialButtonStyle::Outlined,
                                                        palette,
                                                        true,
                                                    )
                                                    .clicked();
                                                }
                                            });
                                            ui.label(
                                                RichText::new(
                                                    "The default opens at startup; switching changes only the current session.",
                                                )
                                                .size(9.0)
                                                .color(palette.palette_hint),
                                            );
                                            ui.add_space(5.0);
                                            ui.horizontal(|ui| {
                                                ui.label("Name");
                                                ui.add(
                                                    TextEdit::singleline(&mut edit_name)
                                                        .desired_width(240.0),
                                                );
                                            });
                                            ui.add_space(5.0);
                                            ui.label(
                                                RichText::new("ICON")
                                                    .size(9.0)
                                                    .color(palette.section_label),
                                            );
                                            ui.horizontal_wrapped(|ui| {
                                                for icon in WorkspaceIcon::ALL {
                                                    if ui
                                                        .selectable_label(edit_icon == icon, icon.label())
                                                        .clicked()
                                                    {
                                                        edit_icon = icon;
                                                    }
                                                }
                                            });
                                            ui.add_space(5.0);
                                            ui.label(
                                                RichText::new("ACCENT")
                                                    .size(9.0)
                                                    .color(palette.section_label),
                                            );
                                            ui.horizontal_wrapped(|ui| {
                                                for color in WorkspaceColor::ALL {
                                                    let (r, g, b) = color.rgb();
                                                    let swatch =
                                                        RichText::new(format!("● {}", color.label()))
                                                            .color(Color32::from_rgb(r, g, b));
                                                    if ui
                                                        .selectable_label(edit_color == color, swatch)
                                                        .clicked()
                                                    {
                                                        edit_color = color;
                                                    }
                                                }
                                            });
                                            ui.add_space(6.0);
                                            ui.horizontal(|ui| {
                                                save_clicked = material_button(
                                                    ui,
                                                    RailIcon::Check,
                                                    "Save changes",
                                                    MaterialButtonStyle::Filled,
                                                    palette,
                                                    true,
                                                )
                                                .clicked();
                                                delete_clicked = material_button(
                                                    ui,
                                                    RailIcon::Delete,
                                                    "Delete",
                                                    MaterialButtonStyle::Text,
                                                    palette,
                                                    workspace_count > 1,
                                                )
                                                .clicked();
                                            });
                                            ui.label(
                                                RichText::new(
                                                    "Saving renames this workspace's bookmark folder. Deleting archives its bookmarks instead of removing them.",
                                                )
                                                .size(10.0)
                                                .color(palette.palette_hint),
                                            );
                                        }
                                    }
                                }
                                SettingsTab::Bookmarks => {
                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("BOOKMARKS")
                                        .size(10.0)
                                        .strong()
                                        .color(palette.section_label),
                                );
                                ui.add_space(3.0);
                                if material_button(
                                    ui,
                                    RailIcon::BookmarkAdd,
                                    "Bookmark current tab",
                                    MaterialButtonStyle::Tonal,
                                    palette,
                                    active_tab_id.is_some(),
                                )
                                .clicked()
                                {
                                    if let Some(tab_id) = active_tab_id {
                                        workspace_actions
                                            .push(UiAction::AddBookmark { tab_id });
                                    }
                                }
                                ui.label(
                                    RichText::new(
                                        "Bookmarks stay visible from every workspace; folders are named after workspaces.",
                                    )
                                    .size(10.0)
                                    .color(palette.palette_hint),
                                );

                                let mut folders: BTreeMap<String, Vec<BookmarkInfo>> =
                                    BTreeMap::new();
                                for bookmark in &bookmarks {
                                    folders
                                        .entry(bookmark.folder.clone())
                                        .or_default()
                                        .push(bookmark.clone());
                                }
                                for (folder, items) in folders {
                                    let heading = format!("{}  ·  {}", folder, items.len());
                                    egui::CollapsingHeader::new(heading)
                                        .id_salt(("bookmark_folder", folder.clone()))
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            for bookmark in items {
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(truncate_title(
                                                            &bookmark.title,
                                                            26,
                                                        ))
                                                        .size(11.0),
                                                    );
                                                    ui.label(
                                                        RichText::new(truncate_title(
                                                            &bookmark.url,
                                                            38,
                                                        ))
                                                        .size(9.0)
                                                        .color(palette.palette_hint),
                                                    );
                                                    ui.with_layout(
                                                        Layout::right_to_left(Align::Center),
                                                        |ui| {
                                                            if material_button(
                                                                ui,
                                                                RailIcon::OpenInNew,
                                                                "Open",
                                                                MaterialButtonStyle::Outlined,
                                                                palette,
                                                                true,
                                                            )
                                                            .clicked()
                                                            {
                                                                workspace_actions.push(
                                                                    UiAction::OpenBookmark {
                                                                        url: bookmark.url.clone(),
                                                                    },
                                                                );
                                                            }
                                                        },
                                                    );
                                                });
                                            }
                                        });
                                }
                                    }
                                }
                            });

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Changes save automatically")
                                    .size(10.0)
                                    .color(palette.palette_hint),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if material_button(
                                    ui,
                                    RailIcon::Settings,
                                    "Reset to defaults",
                                    MaterialButtonStyle::Text,
                                    palette,
                                    true,
                                )
                                .clicked()
                                {
                                    draft = BrowserPreferences::default();
                                }
                            });
                        });
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        if settings_tab != state.settings_tab && settings_tab != SettingsTab::Workspaces {
            state.workspace_create_mode = false;
            state.reset_workspace_create_draft();
        }
        state.settings_tab = settings_tab;
        state.workspace_create_mode = workspace_create_mode;

        let next = draft.normalized();
        state.settings_draft = next.clone();
        if next != state.preferences {
            actions.push(UiAction::ApplyPreferences(next.clone()));
            state.set_preferences(next);
        }

        state.selected_workspace_id = selected_workspace_id;
        state.workspace_edit_name_draft = edit_name.clone();
        state.workspace_edit_icon_draft = edit_icon;
        state.workspace_edit_color_draft = edit_color;
        state.workspace_new_name_draft = new_name.clone();
        state.workspace_new_icon_draft = new_icon;
        state.workspace_new_color_draft = new_color;

        if make_default_clicked {
            if let Some(workspace) = &selected_workspace {
                workspace_actions.push(UiAction::SetDefaultWorkspace {
                    workspace_id: workspace.id,
                });
            }
        }
        if switch_clicked {
            if let Some(workspace) = &selected_workspace {
                workspace_actions.push(UiAction::SwitchWorkspace {
                    workspace_id: workspace.id,
                });
            }
        }
        actions.extend(workspace_form_actions(
            selected_workspace.as_ref(),
            &edit_name,
            edit_icon,
            edit_color,
            save_clicked,
            &new_name,
            new_icon,
            new_color,
            create_clicked,
            delete_clicked,
        ));
        actions.extend(workspace_actions);

        if cancel_create_clicked {
            state.close_workspace_creator();
        }
        if create_clicked {
            // Creation returns to the manage list. Clear the selection so the
            // next frame follows the newly created, now-active workspace.
            state.close_workspace_creator();
            state.follow_active_workspace();
        }
        if delete_clicked {
            state.follow_active_workspace();
        }

        let outside_click = ctx.input(|i| {
            command_palette_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        });
        if workspace_create_mode && requested_close && !create_clicked {
            // Escape in the create form goes back instead of closing Settings.
            state.close_workspace_creator();
        } else if requested_close || done_clicked || outside_click {
            state.close_settings();
        }

        if new_workspace_clicked {
            state.open_workspace_creator();
        }
    }

    /// Record a tooltip for the hovered rail row. It is painted later by a
    /// transparent overlay window so it appears above the native CEF view.
    fn chrome_tooltip(
        tooltip: &mut Option<RailTooltip>,
        ui: &Ui,
        response: &Response,
        text: impl Into<String>,
        expanded: bool,
    ) {
        // Expanded controls already draw visible labels. Tooltips are reserved
        // for the collapsed icon rail so they act as the button labels there.
        if !rail_tooltip_enabled(expanded) || !response.hovered() {
            return;
        }

        let rail = ui.clip_rect();
        let screen = ui.ctx().screen_rect();
        let (width, height) = rail_tooltip_metrics(expanded);
        let anchor = page_tooltip_anchor(rail, response.rect, screen, width, height);
        *tooltip = Some(RailTooltip {
            text: text.into(),
            anchor,
            width,
            height,
        });
    }

    /// Draw a cached favicon into an explicit rect, or a vector globe fallback.
    /// Loading tabs use the same slot for an animated spinner so the favicon
    /// itself communicates navigation progress.
    fn paint_favicon_in_rect(
        &self,
        ui: &mut Ui,
        tab: &TabInfo,
        rect: egui::Rect,
        palette: &ChromePalette,
    ) {
        if !ui.is_rect_visible(rect) {
            return;
        }

        if tab.is_loading {
            paint_loading_spinner(ui, rect, palette);
            return;
        }

        if let Some(favicon) = &tab.favicon {
            if let Some(texture) = self.favicon_textures.get(&favicon.url) {
                let uv = egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                ui.painter().image(texture.id(), rect, uv, Color32::WHITE);
                return;
            }
        }

        paint_rail_icon(ui, rect.shrink(2.0), RailIcon::Globe, palette.text_dim);
    }

    /// A compact square icon button used by the collapsed rail and expanded
    /// header. The hover state uses the same palette as tab rows.
    fn rail_icon_button(
        ui: &mut Ui,
        size: egui::Vec2,
        icon: RailIcon,
        palette: &ChromePalette,
    ) -> Response {
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        let fill = if response.hovered() {
            palette.rail_surface
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, Rounding::same(7.0), fill);
        paint_rail_icon(
            ui,
            rect.shrink(5.0),
            icon,
            if response.hovered() {
                palette.text_strong
            } else {
                palette.text
            },
        );
        response
    }

    /// A horizontal icon + label button for the expanded rail.
    fn rail_action_button(
        ui: &mut Ui,
        width: f32,
        height: f32,
        icon: RailIcon,
        label: &str,
        palette: &ChromePalette,
    ) -> Response {
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        let fill = if response.hovered() {
            palette.rail_surface
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, Rounding::same(7.0), fill);
        let content_color = if response.hovered() {
            palette.text_strong
        } else {
            palette.text
        };
        paint_rail_icon(
            ui,
            egui::Rect::from_center_size(
                pos2(rect.min.x + 17.0, rect.center().y),
                vec2(18.0, 18.0),
            ),
            icon,
            content_color,
        );
        ui.painter().text(
            pos2(rect.min.x + 34.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::new(12.0, FontFamily::Proportional),
            content_color,
        );
        response
    }

    /// Hover-only close affordance. The close hit area is checked before the
    /// parent row click so clicking × never also selects the tab.
    fn paint_tab_close(
        ui: &Ui,
        row_rect: egui::Rect,
        row_hovered: bool,
        tab_id: u64,
        expanded: bool,
        palette: &ChromePalette,
    ) -> Response {
        let size = if expanded { 18.0 } else { 14.0 };
        let rect =
            egui::Rect::from_center_size(tab_close_center(row_rect, expanded), vec2(size, size));
        let response = ui.interact(
            rect,
            egui::Id::new(("kasumi_tab_close", tab_id, expanded)),
            Sense::click(),
        );
        if row_hovered || response.hovered() {
            ui.painter()
                .rect_filled(rect, Rounding::same(4.0), palette.close_bg);
            paint_rail_icon(ui, rect.shrink(2.0), RailIcon::Close, palette.close_icon);
        }
        response
    }

    /// Shared tab-row background. Drawing this against the same rect as the
    /// row contents is what keeps the active/hover highlight aligned.
    fn paint_tab_row(
        ui: &Ui,
        rect: egui::Rect,
        is_active: bool,
        hovered: bool,
        palette: &ChromePalette,
    ) {
        let fill = if is_active {
            palette.rail_row_active
        } else if hovered {
            palette.rail_row_hover
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, Rounding::same(7.0), fill);

        if is_active {
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                Rounding::same(7.0),
                Stroke::new(1.0_f32, palette.tab_border),
            );
            ui.painter().rect_filled(
                egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
                Rounding::same(1.5),
                palette.rail_accent,
            );
        }
    }

    /// Hibernated marker anchored inside the tab row. Loading is now shown by
    /// the spinner in the favicon slot, so the row no longer duplicates it.
    /// The marker shifts left while the close button is visible so the two
    /// never overlap.
    fn paint_tab_status(
        ui: &Ui,
        row_rect: egui::Rect,
        tab: &TabInfo,
        close_visible: bool,
        palette: &ChromePalette,
    ) {
        if tab.display_state != TabDisplayState::Hibernated {
            return;
        }

        let center = tab_status_center(row_rect, close_visible);
        paint_rail_icon(
            ui,
            egui::Rect::from_center_size(center, vec2(12.0, 12.0)),
            RailIcon::Moon,
            palette.text_dim,
        );
    }

    /// Draw one rail tab row. Pinned rows are shorter and replace the hover
    /// close control with a clickable pin badge, while keeping the same
    /// click-to-select and right-click menu behavior as normal tabs.
    #[allow(clippy::too_many_arguments)]
    fn rail_tab_row(
        &self,
        ui: &mut Ui,
        tab: &TabInfo,
        state: &ChromeState,
        actions: &mut Vec<UiAction>,
        tooltip: &mut Option<RailTooltip>,
        menu_request: &mut Option<(u64, egui::Pos2)>,
        rail_rect: egui::Rect,
        palette: &ChromePalette,
        width: f32,
        expanded: bool,
    ) {
        let tab_id = tab.id;
        let is_active = state.active_tab_id == Some(tab_id);
        let row_height = if tab.is_pinned {
            if expanded {
                28.0
            } else {
                36.0
            }
        } else if expanded {
            36.0
        } else {
            44.0
        };
        let horizontal_inset = if expanded { 12.0 } else { 6.0 };
        let (row_rect, row_resp) =
            ui.allocate_exact_size(vec2(width - horizontal_inset, row_height), Sense::click());

        Self::paint_tab_row(ui, row_rect, is_active, row_resp.hovered(), palette);

        let icon_size = if tab.is_pinned {
            if expanded {
                16.0
            } else {
                20.0
            }
        } else if expanded {
            20.0
        } else {
            24.0
        };
        let icon_rect = if expanded {
            egui::Rect::from_min_size(
                pos2(row_rect.min.x + 8.0, row_rect.center().y - icon_size * 0.5),
                vec2(icon_size, icon_size),
            )
        } else {
            egui::Rect::from_center_size(row_rect.center(), vec2(icon_size, icon_size))
        };
        self.paint_favicon_in_rect(ui, tab, icon_rect, palette);

        if expanded {
            let title = if tab.title.is_empty() {
                "New Tab".to_string()
            } else {
                truncate_title(&tab.title, if tab.is_pinned { 13 } else { 18 })
            };
            let text_color = if is_active {
                palette.text_strong
            } else {
                palette.text
            };
            ui.painter().text(
                pos2(
                    row_rect.min.x + if tab.is_pinned { 30.0 } else { 34.0 },
                    row_rect.center().y,
                ),
                Align2::LEFT_CENTER,
                title,
                FontId::new(
                    if tab.is_pinned { 11.0 } else { 12.0 },
                    FontFamily::Proportional,
                ),
                text_color,
            );
        }

        let mut close_resp = None;
        let mut pin_resp = None;
        if tab.is_pinned {
            let size = if expanded { 14.0 } else { 12.0 };
            let badge_rect = egui::Rect::from_center_size(
                tab_close_center(row_rect, expanded),
                vec2(size, size),
            );
            let response = ui.interact(
                badge_rect,
                egui::Id::new(("kasumi_tab_pin", tab_id, expanded)),
                Sense::click(),
            );
            paint_rail_icon(
                ui,
                badge_rect.shrink(1.0),
                RailIcon::Pin,
                palette.rail_accent,
            );
            Self::chrome_tooltip(
                tooltip,
                ui,
                &response,
                format!("Unpin tab: {}", tab.title),
                expanded,
            );
            pin_resp = Some(response);
        } else {
            let response =
                Self::paint_tab_close(ui, row_rect, row_resp.hovered(), tab_id, expanded, palette);
            Self::chrome_tooltip(
                tooltip,
                ui,
                &response,
                format!("Close tab: {}", tab.title),
                expanded,
            );
            close_resp = Some(response);
        }

        Self::paint_tab_status(
            ui,
            row_rect,
            tab,
            tab.is_pinned
                || row_resp.hovered()
                || close_resp
                    .as_ref()
                    .is_some_and(|response| response.hovered()),
            palette,
        );

        if pin_resp.as_ref().is_some_and(|response| response.clicked()) {
            actions.push(UiAction::TogglePinTab { tab_id });
        } else if close_resp
            .as_ref()
            .is_some_and(|response| response.clicked())
        {
            actions.push(UiAction::CloseTab { tab_id });
        } else if row_resp.clicked() {
            actions.push(UiAction::SelectTab { tab_id });
        }
        if !tab.is_pinned && row_resp.middle_clicked() {
            actions.push(UiAction::CloseTab { tab_id });
        }
        if row_resp.secondary_clicked() {
            let pointer = ui
                .ctx()
                .pointer_latest_pos()
                .unwrap_or(row_rect.right_top());
            let (menu_width, menu_height) = rail_menu_metrics();
            let anchor = rail_menu_anchor(
                rail_rect,
                pointer,
                ui.ctx().screen_rect(),
                menu_width,
                menu_height,
            );
            *menu_request = Some((tab_id, anchor));
        }

        let label = if tab.is_pinned {
            format!("Pinned · {}", tab.title)
        } else {
            tab.title.clone()
        };
        let tooltip_text = if expanded {
            format!("{}\n{}", label, tab.url)
        } else {
            label
        };
        Self::chrome_tooltip(tooltip, ui, &row_resp, tooltip_text, expanded);
    }
}

// ---------------------------------------------------------------------------
// Palette overlay renderer
// ---------------------------------------------------------------------------

/// Owns the transparent top-level window shared by the command palette and
/// the rail tab context menu.
///
/// The CEF child view is a native view above the main window's wgpu layer, so
/// an in-window egui overlay would be covered by the page. A separate native
/// window lets the page stay visible behind a real overlay on every platform.
pub struct ChromeOverlayRenderer {
    surface: wgpu::Surface<'static>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    max_texture_dim: u32,
}

impl ChromeOverlayRenderer {
    pub fn new(
        window: Arc<winit::window::Window>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> Option<Self> {
        let surface = match instance.create_surface(Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(e) => {
                tracing::error!("Failed to create palette overlay surface: {e}");
                return None;
            }
        };

        let size = window.inner_size();
        let caps = surface.get_capabilities(adapter);
        let Some(format) = preferred_surface_format(&caps.formats) else {
            tracing::error!("Palette overlay adapter reported no surface formats");
            return None;
        };
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| {
                matches!(
                    m,
                    wgpu::CompositeAlphaMode::PreMultiplied
                        | wgpu::CompositeAlphaMode::PostMultiplied
                )
            })
            .unwrap_or(wgpu::CompositeAlphaMode::Opaque);

        let max_tex = adapter.limits().max_texture_dimension_2d;
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1).min(max_tex),
            height: size.height.max(1).min(max_tex),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        let egui_ctx = egui::Context::default();
        egui_material_icons::initialize(&egui_ctx);

        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(max_tex as usize),
        );

        Some(Self {
            surface,
            device,
            queue,
            surface_config,
            renderer,
            egui_ctx,
            egui_state,
            max_texture_dim: max_tex,
        })
    }

    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
        self.egui_state.on_window_event(window, event).consumed
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.surface_config.width = new_size.width.min(self.max_texture_dim);
        self.surface_config.height = new_size.height.min(self.max_texture_dim);
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Render the palette or tab menu into the transparent overlay window.
    pub fn render(
        &mut self,
        window: &winit::window::Window,
        state: &mut ChromeState,
    ) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let mut raw_input = self.egui_state.take_egui_input(window);
        // Only one surface may own the shared overlay. Settings has priority,
        // then the context menu, then the command palette, then the downloads
        // bubble. `ChromeState` openers are mutually exclusive, so this is only
        // a defensive ordering for state restored between frames.
        let settings_open = state.settings_open;
        let menu_open = state.menu.is_some() && !settings_open;
        let palette_open = state.command_palette_open && !menu_open && !settings_open;
        let downloads_open = state.downloads_open && !palette_open && !menu_open && !settings_open;
        let focus_before = state.command_palette_focus_requested && palette_open;
        let select_all_before = state.command_palette_select_all_requested && palette_open;
        state.apply_visuals(&self.egui_ctx);

        // Focus is requested while building the input widget, and egui handles
        // key events before that point in the same frame. Select all on the
        // following frame, once the input is reliably focused, so typing a
        // search replaces the active URL instead of appending to it.
        if select_all_before {
            raw_input.events.push(egui::Event::Key {
                key: egui::Key::A,
                physical_key: Some(egui::Key::A),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
        }
        let focus_command = focus_before;

        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            if settings_open {
                ChromeRenderer::build_settings(ctx, state, &mut actions);
            } else if menu_open {
                ChromeRenderer::build_tab_menu(ctx, state, &mut actions);
            } else if palette_open {
                ChromeRenderer::build_command_palette(ctx, state, &mut actions, focus_command);
            } else if downloads_open {
                ChromeRenderer::build_downloads_bubble(ctx, state, &mut actions);
            }
        });
        // The rail button raises this flag during the main window's render,
        // which happens before this overlay's first frame. Consume it here.
        let select_all_next = focus_before && palette_open;
        state.command_palette_focus_requested =
            focus_request_after_render(focus_before, state.command_palette_focus_requested);
        state.command_palette_select_all_requested = select_all_next;

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        let output_frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(e) => {
                tracing::warn!("palette surface error: {:?}", e);
                return actions;
            }
        };
        let view = output_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: window.scale_factor() as f32,
        };
        let tris = self
            .egui_ctx
            .tessellate(full_output.shapes, screen_descriptor.pixels_per_point);

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("palette_overlay"),
            });
        self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &tris,
            &screen_descriptor,
        );

        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("palette_overlay_rpass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 0.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        output_frame.present();

        actions
    }
}

// ---------------------------------------------------------------------------
// Tooltip overlay renderer
// ---------------------------------------------------------------------------

/// Transparent, click-through window used to paint rail tooltips over the
/// native CEF page view.
pub struct TooltipRenderer {
    surface: wgpu::Surface<'static>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    max_texture_dim: u32,
}

impl TooltipRenderer {
    pub fn new(
        window: Arc<winit::window::Window>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> Option<Self> {
        let surface = match instance.create_surface(Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(e) => {
                tracing::error!("Failed to create tooltip overlay surface: {e}");
                return None;
            }
        };

        let size = window.inner_size();
        let caps = surface.get_capabilities(adapter);
        let Some(format) = preferred_surface_format(&caps.formats) else {
            tracing::error!("Tooltip overlay adapter reported no surface formats");
            return None;
        };
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| {
                matches!(
                    m,
                    wgpu::CompositeAlphaMode::PreMultiplied
                        | wgpu::CompositeAlphaMode::PostMultiplied
                )
            })
            .unwrap_or(wgpu::CompositeAlphaMode::Opaque);

        let max_tex = adapter.limits().max_texture_dimension_2d;
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1).min(max_tex),
            height: size.height.max(1).min(max_tex),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(max_tex as usize),
        );

        Some(Self {
            surface,
            device,
            queue,
            surface_config,
            renderer,
            egui_ctx,
            egui_state,
            max_texture_dim: max_tex,
        })
    }

    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
        self.egui_state.on_window_event(window, event).consumed
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0
            || new_size.height == 0
            || (new_size.width == self.surface_config.width
                && new_size.height == self.surface_config.height)
        {
            return;
        }
        self.surface_config.width = new_size.width.min(self.max_texture_dim);
        self.surface_config.height = new_size.height.min(self.max_texture_dim);
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Paint the tooltip card into the transparent overlay window.
    pub fn render(
        &mut self,
        window: &winit::window::Window,
        tooltip: &RailTooltip,
        state: &ChromeState,
    ) {
        self.resize(window.inner_size());
        state.apply_visuals(&self.egui_ctx);
        let palette = state.palette();
        let raw_input = self.egui_state.take_egui_input(window);
        let card = tooltip.clone();
        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default()
                .frame(Frame::none().fill(Color32::TRANSPARENT))
                .show(ctx, |ui| {
                    Frame::none()
                        .fill(palette.tooltip_bg)
                        .stroke(Stroke::new(1.0_f32, palette.tooltip_border))
                        .rounding(Rounding::same(6.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                        .show(ui, |ui| {
                            // Fill the whole overlay window so the visible card is
                            // vertically centered on the hovered tab row. A plain
                            // label sizes the frame to its text and leaves the card
                            // pinned near the top of the taller transparent window.
                            let inner_size = vec2(
                                (card.width - 16.0).max(80.0),
                                (card.height - 12.0).max(10.0),
                            );
                            let (rect, _) = ui.allocate_exact_size(inner_size, Sense::hover());
                            let lines: Vec<&str> = card.text.lines().collect();
                            let line_height = 15.0_f32;
                            let total_height = lines.len() as f32 * line_height;
                            let first_line_y =
                                rect.center().y - total_height * 0.5 + line_height * 0.5;
                            for (index, line) in lines.iter().enumerate() {
                                ui.painter().text(
                                    pos2(
                                        rect.center().x,
                                        first_line_y + index as f32 * line_height,
                                    ),
                                    Align2::CENTER_CENTER,
                                    *line,
                                    FontId::proportional(11.0),
                                    palette.tooltip_text,
                                );
                            }
                        });
                });
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        let output_frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(e) => {
                tracing::warn!("tooltip surface error: {:?}", e);
                return;
            }
        };
        let view = output_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: window.scale_factor() as f32,
        };
        let tris = self
            .egui_ctx
            .tessellate(full_output.shapes, screen_descriptor.pixels_per_point);

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tooltip_overlay"),
            });
        self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &tris,
            &screen_descriptor,
        );

        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tooltip_overlay_rpass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 0.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        output_frame.present();
    }
}

/// Draw one of the rail's icons inside `rect` using the registered Material
/// Symbols font. Font glyphs stay crisp at any DPI and give the rail a
/// consistent, familiar icon language.
fn rail_icon_glyph(icon: RailIcon) -> &'static str {
    use egui_material_icons::icons::{
        ICON_ADD, ICON_BEDTIME, ICON_BOOK, ICON_BOOKMARK_ADD, ICON_CHECK, ICON_CHEVRON_LEFT,
        ICON_CHEVRON_RIGHT, ICON_CLOSE, ICON_CODE, ICON_CONTENT_COPY, ICON_DELETE, ICON_DOWNLOAD,
        ICON_EDIT, ICON_FOLDER_OPEN, ICON_MEMORY, ICON_MUSIC_NOTE, ICON_OPEN_IN_NEW, ICON_PALETTE,
        ICON_PAUSE, ICON_PIN, ICON_PLAY_ARROW, ICON_PUBLIC, ICON_PUSH_PIN, ICON_REFRESH,
        ICON_ROCKET, ICON_SCHOOL, ICON_SEARCH, ICON_SETTINGS, ICON_SHIELD, ICON_STAR,
        ICON_VIEW_SIDEBAR, ICON_WORK,
    };

    match icon {
        RailIcon::Plus => ICON_ADD,
        RailIcon::Search => ICON_SEARCH,
        RailIcon::Code => ICON_CODE,
        RailIcon::ChevronLeft => ICON_CHEVRON_LEFT,
        RailIcon::ChevronRight => ICON_CHEVRON_RIGHT,
        RailIcon::Moon => ICON_BEDTIME,
        RailIcon::Shield => ICON_SHIELD,
        RailIcon::Chip => ICON_MEMORY,
        RailIcon::Globe => ICON_PUBLIC,
        RailIcon::Close => ICON_CLOSE,
        RailIcon::Pin => ICON_PUSH_PIN,
        RailIcon::PinOff => ICON_PIN,
        RailIcon::Copy => ICON_CONTENT_COPY,
        RailIcon::Sidebar => ICON_VIEW_SIDEBAR,
        RailIcon::Settings => ICON_SETTINGS,
        RailIcon::Download => ICON_DOWNLOAD,
        RailIcon::Pause => ICON_PAUSE,
        RailIcon::Play => ICON_PLAY_ARROW,
        RailIcon::Retry => ICON_REFRESH,
        RailIcon::FolderOpen => ICON_FOLDER_OPEN,
        RailIcon::Check => ICON_CHECK,
        RailIcon::Delete => ICON_DELETE,
        RailIcon::Star => ICON_STAR,
        RailIcon::Edit => ICON_EDIT,
        RailIcon::BookmarkAdd => ICON_BOOKMARK_ADD,
        RailIcon::OpenInNew => ICON_OPEN_IN_NEW,
        RailIcon::WorkspaceWork => ICON_WORK,
        RailIcon::WorkspaceCode => ICON_CODE,
        RailIcon::WorkspaceStudy => ICON_SCHOOL,
        RailIcon::WorkspaceLaunch => ICON_ROCKET,
        RailIcon::WorkspaceDesign => ICON_PALETTE,
        RailIcon::WorkspaceMusic => ICON_MUSIC_NOTE,
        RailIcon::WorkspaceReading => ICON_BOOK,
        RailIcon::WorkspaceWeb => ICON_PUBLIC,
    }
}

fn paint_rail_icon(ui: &Ui, rect: egui::Rect, icon: RailIcon, color: Color32) {
    let glyph = rail_icon_glyph(icon);
    let size = rect.width().min(rect.height()) * 1.08;
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::new(size, FontFamily::Proportional),
        color,
    );
}

/// A compact Material-style button: a registered Symbols glyph followed by a
/// text label. Shared by Settings actions so create/save/delete never look like
/// ambiguous text-only links.
fn material_button(
    ui: &mut Ui,
    icon: RailIcon,
    label: &str,
    style: MaterialButtonStyle,
    palette: &ChromePalette,
    enabled: bool,
) -> Response {
    let (fill, stroke, color) = match style {
        MaterialButtonStyle::Filled => (
            palette.rail_row_active,
            Stroke::new(1.0_f32, palette.rail_accent),
            palette.text_strong,
        ),
        MaterialButtonStyle::Tonal => (
            palette.rail_surface,
            Stroke::new(1.0_f32, palette.palette_border),
            palette.text_strong,
        ),
        MaterialButtonStyle::Outlined => (
            Color32::TRANSPARENT,
            Stroke::new(1.0_f32, palette.palette_border),
            palette.text,
        ),
        MaterialButtonStyle::Text => (
            Color32::TRANSPARENT,
            Stroke::new(0.0_f32, Color32::TRANSPARENT),
            palette.text,
        ),
    };
    let text = RichText::new(format!("{}  {}", rail_icon_glyph(icon), label))
        .size(11.5)
        .color(color);
    let button = egui::Button::new(text)
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(8.0))
        .min_size(vec2(0.0, 28.0));
    ui.add_enabled(enabled, button)
}

/// A small, transparent icon-only button used by dense download rows. Hover
/// feedback comes from the glyph colour and egui's own tooltip, so no filled
/// background competes with the transfer progress text.
fn compact_icon_button(
    ui: &mut Ui,
    icon: RailIcon,
    label: &'static str,
    palette: &ChromePalette,
    prominent: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::click());
    let color = if prominent || response.hovered() {
        palette.text_strong
    } else {
        palette.text
    };
    let icon_rect = egui::Rect::from_center_size(rect.center(), vec2(14.0, 14.0));
    paint_rail_icon(ui, icon_rect, icon, color);
    response.on_hover_text(label)
}

/// Paint the active-transfer count (or a quiet history dot) over the rail's
/// Downloads button. It is drawn directly on the native rail, not in a second
/// view, so it tracks download snapshots on the same frame as the bubble.
fn paint_download_badge(
    ui: &Ui,
    rect: egui::Rect,
    badge: DownloadBadge,
    palette: &ChromePalette,
    collapsed: bool,
) {
    let inset = if collapsed { 4.0 } else { 8.0 };
    let center = pos2(rect.right() - inset, rect.top() + inset);
    match badge {
        DownloadBadge::Hidden => {}
        DownloadBadge::History => {
            ui.painter().circle_filled(center, 4.0, palette.shield_ok);
        }
        DownloadBadge::Active(count) => {
            let radius = if collapsed { 7.5 } else { 9.0 };
            let text = if count > 99 {
                "99+".to_string()
            } else {
                count.to_string()
            };
            ui.painter()
                .circle_filled(center, radius, palette.rail_accent);
            ui.painter().text(
                center,
                Align2::CENTER_CENTER,
                text,
                FontId::new(9.0, FontFamily::Proportional),
                palette.text_strong,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Pure chrome helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Tooltips are labels for the icon-only collapsed rail. Expanded controls
/// already draw visible text, so expanded-mode tooltips are disabled.
pub fn rail_tooltip_enabled(expanded: bool) -> bool {
    !expanded
}

/// Tooltip width/height for each rail mode. Both modes use a readable card
/// placed over the page; expanded rows include the URL and need more height.
pub fn rail_tooltip_metrics(expanded: bool) -> (f32, f32) {
    if expanded {
        (TOOLTIP_WIDTH, TOOLTIP_HEIGHT_EXPANDED)
    } else {
        (TOOLTIP_WIDTH, TOOLTIP_HEIGHT_COLLAPSED)
    }
}

/// Icon size for the RAM/ad-block status rows. The collapsed rail uses larger
/// glyphs so both badges stay legible at 44 px wide; expanded rows keep the
/// compact icon because their labels carry the value.
pub fn rail_status_icon_size(expanded: bool) -> f32 {
    if expanded {
        RAIL_STATUS_ICON_EXPANDED
    } else {
        RAIL_STATUS_ICON_COLLAPSED
    }
}

/// Fixed card size for the rail tab context menu.
pub fn rail_menu_metrics() -> (f32, f32) {
    const ITEM_COUNT: usize = 10;
    (
        RAIL_MENU_WIDTH,
        ITEM_COUNT as f32 * RAIL_MENU_ROW_HEIGHT + RAIL_MENU_PADDING * 2.0,
    )
}

/// Place the context menu beside the rail, near the right-click, and clamp it
/// inside the visible window. This is the card's top-left in main-window
/// logical coordinates.
pub fn rail_menu_anchor(
    rail: egui::Rect,
    pointer: egui::Pos2,
    screen: egui::Rect,
    menu_width: f32,
    menu_height: f32,
) -> egui::Pos2 {
    let min_x = rail.right() + 8.0;
    let max_x = (screen.right() - menu_width - 6.0).max(min_x);
    let x = pointer.x.clamp(min_x, max_x);
    let max_y = (screen.bottom() - menu_height - 6.0).max(screen.top() + 6.0);
    let y = (pointer.y - 8.0).clamp(screen.top() + 6.0, max_y);
    pos2(x, y)
}

/// Fixed card size for the downloads bubble. Both dimensions stay finite so
/// egui can cache the `Area` size without poisoning later frames.
pub fn downloads_bubble_size(screen: egui::Rect) -> egui::Vec2 {
    vec2(
        DOWNLOADS_WIDTH.min((screen.width() - 24.0).max(180.0)),
        DOWNLOADS_MAX_HEIGHT.min((screen.height() - 24.0).max(140.0)),
    )
}

/// Anchor the downloads bubble beside the rail near the bottom of the window,
/// where its rail button lives in both rail modes. The position is clamped to
/// the visible screen so small windows never push the card off-screen.
pub fn downloads_bubble_anchor(screen: egui::Rect, rail_right: f32) -> egui::Pos2 {
    let size = downloads_bubble_size(screen);
    let min_x = screen.left() + 4.0;
    let max_x = (screen.right() - size.x - 4.0).max(min_x);
    let x = (rail_right + DOWNLOADS_RAIL_GAP).clamp(min_x, max_x);
    let max_y = (screen.bottom() - size.y - 4.0).max(screen.top() + 4.0);
    let y = (screen.bottom() - DOWNLOADS_BOTTOM_MARGIN - size.y).clamp(screen.top() + 4.0, max_y);
    pos2(x, y)
}

/// True when a click landed outside the context-menu card.
pub fn rail_menu_click_outside(
    card: egui::Rect,
    pointer_pos: Option<egui::Pos2>,
    primary_clicked: bool,
) -> bool {
    primary_clicked && pointer_pos.is_some_and(|p| !card.contains(p))
}

/// Build the context-menu rows for the right-clicked tab.
pub fn rail_menu_items(state: &ChromeState, tab_id: u64) -> Vec<RailMenuItem> {
    let workspace_tabs: Vec<&TabInfo> = state
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id == state.active_workspace_id)
        .collect();
    let index = workspace_tabs.iter().position(|tab| tab.id == tab_id);
    let tab_count = workspace_tabs.len();
    let is_pinned = workspace_tabs
        .iter()
        .find(|tab| tab.id == tab_id)
        .is_some_and(|tab| tab.is_pinned);
    let pin_label = if is_pinned { "Unpin tab" } else { "Pin tab" };
    let item = |label, action, enabled| RailMenuItem {
        label,
        action,
        enabled,
    };
    vec![
        item("New tab", RailMenuAction::NewTab, true),
        item("Duplicate tab", RailMenuAction::DuplicateTab, true),
        item(pin_label, RailMenuAction::TogglePinTab, true),
        item("Copy URL", RailMenuAction::CopyUrl, true),
        item("Reload tab", RailMenuAction::Reload, true),
        item("Close tab", RailMenuAction::CloseTab, true),
        item(
            "Close other tabs",
            RailMenuAction::CloseOtherTabs,
            tab_count > 1,
        ),
        item(
            "Close tabs below this tab",
            RailMenuAction::CloseTabsToRight,
            index.is_some_and(|i| i + 1 < tab_count),
        ),
        item(
            "Reopen closed tab (Cmd/Ctrl+Z)",
            RailMenuAction::ReopenClosedTab,
            state.can_reopen_tab,
        ),
        item("DevTools (new window)", RailMenuAction::OpenDevTools, true),
    ]
}

/// Convert a menu command into a tab-manager action. Copying a URL is handled
/// directly by the overlay because it needs the egui clipboard context.
pub fn rail_menu_ui_action(action: RailMenuAction, tab_id: u64) -> Option<UiAction> {
    match action {
        RailMenuAction::NewTab => Some(UiAction::NewTab { url: None }),
        RailMenuAction::DuplicateTab => Some(UiAction::DuplicateTab { tab_id }),
        RailMenuAction::TogglePinTab => Some(UiAction::TogglePinTab { tab_id }),
        RailMenuAction::CopyUrl => None,
        RailMenuAction::Reload => Some(UiAction::Reload {
            tab_id,
            ignore_cache: false,
        }),
        RailMenuAction::CloseTab => Some(UiAction::CloseTab { tab_id }),
        RailMenuAction::CloseOtherTabs => Some(UiAction::CloseOtherTabs { tab_id }),
        RailMenuAction::CloseTabsToRight => Some(UiAction::CloseTabsToRight { tab_id }),
        RailMenuAction::ReopenClosedTab => Some(UiAction::ReopenClosedTab),
        RailMenuAction::OpenDevTools => Some(UiAction::OpenDevTools { tab_id }),
    }
}

const LOADING_SPINNER_SEGMENTS: usize = 12;
const LOADING_SPINNER_BRIGHT_SEGMENTS: usize = 4;
const LOADING_SPINNER_ROTATIONS_PER_SECOND: f32 = 1.1;

/// Angle of one spinner segment. Keeping this pure makes the rotation easy to
/// verify without rendering a wgpu frame.
fn loading_spinner_angle(time_seconds: f32, index: usize, segments: usize) -> f32 {
    std::f32::consts::TAU
        * (time_seconds * LOADING_SPINNER_ROTATIONS_PER_SECOND + index as f32 / segments as f32)
}

fn loading_spinner_point(center: egui::Pos2, radius: f32, angle: f32) -> egui::Pos2 {
    pos2(
        center.x + angle.cos() * radius,
        center.y + angle.sin() * radius,
    )
}

/// Draw the native loading indicator in the favicon slot. A short bright arc
/// rotates around a faint full ring; `request_repaint` keeps it animated only
/// while a loading tab is actually being rendered.
fn paint_loading_spinner(ui: &Ui, rect: egui::Rect, palette: &ChromePalette) {
    let center = rect.center();
    let radius = (rect.width().min(rect.height()) * 0.5 - 2.0).max(3.0);
    let span = std::f32::consts::TAU / LOADING_SPINNER_SEGMENTS as f32;
    let gap = span * 0.3;
    let time = ui.input(|input| input.time) as f32;

    for index in 0..LOADING_SPINNER_SEGMENTS {
        let angle = loading_spinner_angle(time, index, LOADING_SPINNER_SEGMENTS);
        let start = loading_spinner_point(center, radius, angle + gap);
        let end = loading_spinner_point(center, radius, angle + span - gap);
        let intensity = if index < LOADING_SPINNER_BRIGHT_SEGMENTS {
            1.0 - 0.65 * index as f32 / LOADING_SPINNER_BRIGHT_SEGMENTS as f32
        } else {
            0.14
        };
        ui.painter().line_segment(
            [start, end],
            egui::Stroke::new(2.0_f32, palette.loading.gamma_multiply(intensity)),
        );
    }

    ui.ctx().request_repaint();
}

/// Center of the hover-only close button for a tab row.
pub fn tab_close_center(row: egui::Rect, expanded: bool) -> egui::Pos2 {
    if expanded {
        pos2(row.right() - 13.0, row.center().y)
    } else {
        pos2(row.right() - 9.0, row.top() + 9.0)
    }
}

/// Center of the loading/hibernated marker, shifted left when × is shown.
pub fn tab_status_center(row: egui::Rect, close_visible: bool) -> egui::Pos2 {
    pos2(
        row.right() - if close_visible { 30.0 } else { 11.0 },
        row.center().y,
    )
}

/// A focus request raised while rendering a frame must survive to the next
/// frame; one that was already pending has been applied and can be cleared.
pub fn focus_request_after_render(was_requested: bool, is_requested: bool) -> bool {
    !was_requested && is_requested
}

/// A primary click outside the palette card dismisses the overlay. A missing
/// pointer position (touch/cursor just left the window) must not dismiss it.
pub fn command_palette_click_outside(
    palette_rect: egui::Rect,
    pointer_pos: Option<egui::Pos2>,
    primary_clicked: bool,
) -> bool {
    primary_clicked && pointer_pos.is_some_and(|p| !palette_rect.contains(p))
}

/// Place a rail tooltip just to the right of the rail, vertically centered on
/// the hovered row and clamped inside the visible window. This is the top-left
/// corner of the overlay tooltip window in main-window logical coordinates.
pub fn page_tooltip_anchor(
    rail: egui::Rect,
    row: egui::Rect,
    screen: egui::Rect,
    tooltip_width: f32,
    tooltip_height: f32,
) -> egui::Pos2 {
    let min_x = rail.right() + 8.0;
    let max_x = (screen.right() - tooltip_width - 6.0).max(min_x);
    let x = min_x.clamp(min_x, max_x);
    let max_y = (screen.bottom() - tooltip_height - 6.0).max(screen.top() + 6.0);
    let y = (row.center().y - tooltip_height * 0.5).clamp(screen.top() + 6.0, max_y);
    pos2(x, y)
}

/// Truncate a tab title by Unicode scalar value, not bytes, so non-ASCII
/// titles never panic or split a multi-byte character.
pub fn truncate_title(title: &str, max_chars: usize) -> String {
    let mut chars = title.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Keep the selected palette row in bounds without wrapping on empty lists.
pub fn clamp_command_selection(selection: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        selection.min(len - 1)
    }
}

/// Advance the palette highlight from arrow keys.
///
/// Down clamps at the last row; Up wraps to the last row from the first. This
/// is kept pure so keyboard navigation is covered without constructing egui
/// input events. Hovering a row must not call this function: mouse hover only
/// changes the subtle hover tint and must never steal the keyboard selection.
pub fn advance_command_selection(selection: usize, len: usize, up: bool, down: bool) -> usize {
    let current = clamp_command_selection(selection, len);
    if len == 0 {
        return 0;
    }
    if down && !up {
        (current + 1).min(len - 1)
    } else if up && !down {
        current.checked_sub(1).unwrap_or(len - 1)
    } else {
        current
    }
}

/// Selected rows win over hover; hover is only a secondary affordance.
pub fn palette_row_fill(palette: &ChromePalette, selected: bool, hovered: bool) -> Color32 {
    if selected {
        palette.palette_row_selected
    } else if hovered {
        palette.palette_row_hover
    } else {
        Color32::TRANSPARENT
    }
}

/// Build the ordered command palette candidates for the current query.
pub fn command_palette_items(
    query: &str,
    tabs: &[TabInfo],
    active_tab_id: Option<u64>,
    preferences: &BrowserPreferences,
) -> Vec<CommandPaletteItem> {
    let trimmed = query.trim();
    let mut items = Vec::new();

    let tab_item = |tab: &TabInfo| CommandPaletteItem {
        icon: if tab.is_pinned {
            "📌"
        } else if tab.display_state == TabDisplayState::Hibernated {
            "💤"
        } else {
            "◧"
        },
        label: if tab.title.is_empty() {
            "New Tab".to_string()
        } else {
            tab.title.clone()
        },
        subtitle: format!("{} · {}", tab.workspace_name, tab.url),
        action: CommandAction::SelectTab(tab.id),
    };

    let pin_item = || {
        active_tab_id.map(|tab_id| {
            let is_pinned = tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| tab.is_pinned);
            CommandPaletteItem {
                icon: "📌",
                label: if is_pinned {
                    "Unpin active tab"
                } else {
                    "Pin active tab"
                }
                .to_string(),
                subtitle: "Keep this tab in the compact pinned section".to_string(),
                action: CommandAction::TogglePinTab(tab_id),
            }
        })
    };

    if trimmed.is_empty() {
        items.push(CommandPaletteItem {
            icon: "＋",
            label: "New tab".to_string(),
            subtitle: "Open a fresh tab".to_string(),
            action: CommandAction::NewTab(None),
        });
        if let Some(item) = pin_item() {
            items.push(item);
        }
        items.push(CommandPaletteItem {
            icon: "💤",
            label: "Hibernate all background tabs".to_string(),
            subtitle: "Immediately close background browser views".to_string(),
            action: CommandAction::HibernateAll,
        });
        items.push(CommandPaletteItem {
            icon: "▤",
            label: "Toggle workspace rail".to_string(),
            subtitle: "Expand or collapse the left rail".to_string(),
            action: CommandAction::ToggleRail,
        });
        items.push(CommandPaletteItem {
            icon: "⚙",
            label: "Open Settings".to_string(),
            subtitle: "Theme, search, privacy, and performance".to_string(),
            action: CommandAction::OpenSettings,
        });
        items.push(CommandPaletteItem {
            icon: "⇩",
            label: "Open Downloads".to_string(),
            subtitle: "Native parallel downloads and history".to_string(),
            action: CommandAction::OpenDownloads,
        });
        items.extend(tabs.iter().take(12).map(tab_item));
        return items;
    }

    let needle = trimmed.to_lowercase();
    let url = resolve_omnibox_with_search(trimmed, preferences.search_url_prefix());
    let looks_like_address = trimmed.contains('.')
        || trimmed.contains("://")
        || trimmed.starts_with("view-source:")
        || trimmed.starts_with("data:");

    // Put the navigation choices first. A typed query is most often a search,
    // and the new-tab variant must be visible without scrolling past matches.
    if let Some(tab_id) = active_tab_id {
        items.push(CommandPaletteItem {
            icon: "↗",
            label: if looks_like_address {
                "Open in this tab".to_string()
            } else {
                "Search in this tab".to_string()
            },
            subtitle: url.clone(),
            action: CommandAction::Navigate {
                tab_id,
                url: url.clone(),
            },
        });
    }
    items.push(CommandPaletteItem {
        icon: "＋",
        label: if looks_like_address {
            "Open in a new tab".to_string()
        } else {
            "Search in a new tab".to_string()
        },
        subtitle: url,
        action: CommandAction::NewTab(Some(resolve_omnibox_with_search(
            trimmed,
            preferences.search_url_prefix(),
        ))),
    });

    for tab in tabs {
        if tab.title.to_lowercase().contains(&needle) || tab.url.to_lowercase().contains(&needle) {
            items.push(tab_item(tab));
        }
    }

    if needle.contains("devtool") {
        if let Some(tab_id) = active_tab_id {
            items.push(CommandPaletteItem {
                icon: "⌘",
                label: "Open DevTools".to_string(),
                subtitle: "Inspect the active tab in a new window".to_string(),
                action: CommandAction::OpenDevTools,
            });
            let _ = tab_id;
        }
    }

    if needle.contains("setting") || needle.contains("preference") {
        items.push(CommandPaletteItem {
            icon: "⚙",
            label: "Open Settings".to_string(),
            subtitle: "Theme, search, privacy, and performance".to_string(),
            action: CommandAction::OpenSettings,
        });
    }

    if needle.contains("download") {
        items.push(CommandPaletteItem {
            icon: "⇩",
            label: "Open Downloads".to_string(),
            subtitle: "Native parallel downloads and history".to_string(),
            action: CommandAction::OpenDownloads,
        });
    }

    if needle.contains("pin") {
        if let Some(item) = pin_item() {
            items.push(item);
        }
    }

    items
}

fn apply_command_item(
    action: &CommandAction,
    state: &mut ChromeState,
    actions: &mut Vec<UiAction>,
) {
    match action {
        CommandAction::SelectTab(tab_id) => actions.push(UiAction::SelectTab { tab_id: *tab_id }),
        CommandAction::NewTab(url) => actions.push(UiAction::NewTab { url: url.clone() }),
        CommandAction::Navigate { tab_id, url } => actions.push(UiAction::Navigate {
            tab_id: *tab_id,
            url: url.clone(),
        }),
        CommandAction::TogglePinTab(tab_id) => {
            actions.push(UiAction::TogglePinTab { tab_id: *tab_id })
        }
        CommandAction::HibernateAll => actions.push(UiAction::HibernateAllBackground),
        CommandAction::ToggleRail => state.toggle_rail(actions),
        CommandAction::OpenDevTools => {
            if let Some(tab_id) = state.active_tab_id {
                actions.push(UiAction::OpenDevTools { tab_id });
            }
        }
        CommandAction::OpenSettings => state.open_settings(),
        CommandAction::OpenDownloads => state.open_downloads(),
    }
    state.close_command_palette();
}

/// Apply one context-menu command. `CopyUrl` is handled here because it needs
/// the egui clipboard context; every other command is dispatched to TabManager.
fn apply_menu_item(
    ctx: &egui::Context,
    action: &RailMenuAction,
    state: &mut ChromeState,
    actions: &mut Vec<UiAction>,
) {
    if *action == RailMenuAction::CopyUrl {
        if let Some(menu) = state.menu.as_ref() {
            if let Some(tab) = state.tabs.iter().find(|tab| tab.id == menu.tab_id) {
                ctx.copy_text(tab.url.clone());
            }
        }
    } else if let Some(action) =
        rail_menu_ui_action(*action, state.menu.as_ref().map_or(0, |m| m.tab_id))
    {
        actions.push(action);
    }
    state.close_tab_menu();
}

// ---------------------------------------------------------------------------
// Omnibox resolution logic
// ---------------------------------------------------------------------------

/// Convert omnibox input to a navigable URL per AGENTS.md spec.
pub fn resolve_omnibox(input: &str) -> String {
    resolve_omnibox_with_search(input, DEFAULT_SEARCH_URL_PREFIX)
}

/// Same resolution logic with a configurable search provider prefix.
pub fn resolve_omnibox_with_search(input: &str, search_url_prefix: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return crate::config::DEFAULT_HOME_URL.to_string();
    }

    // CEF's view-source: pseudo-scheme is not a normal URL for `url::Url`,
    // but it must survive omnibox input unchanged. URL schemes are
    // case-insensitive.
    if trimmed
        .get(..12)
        .map(|prefix| prefix.eq_ignore_ascii_case("view-source:"))
        .unwrap_or(false)
    {
        return trimmed.to_string();
    }

    if url::Url::parse(trimmed).is_ok() {
        return trimmed.to_string();
    }

    if !trimmed.contains(' ') && trimmed.contains('.') {
        return format!("https://{}", trimmed);
    }

    let encoded = url::form_urlencoded::byte_serialize(trimmed.as_bytes()).collect::<String>();
    format!("{}{}", search_url_prefix, encoded)
}

#[cfg(test)]
mod tests {
    use egui::{pos2, vec2, Color32};

    use super::{
        advance_command_selection, apply_command_item, clamp_command_selection,
        command_palette_click_outside, command_palette_items, download_engine_label,
        download_eta_label, download_rail_badge, download_row_uses_progress_bar,
        download_state_label, downloads_bubble_anchor, downloads_bubble_size, effective_theme,
        focus_request_after_render, format_download_bytes, loading_spinner_angle,
        loading_spinner_point, page_tooltip_anchor, palette_row_fill, preferred_surface_format,
        rail_menu_anchor, rail_menu_click_outside, rail_menu_items, rail_menu_metrics,
        rail_menu_ui_action, rail_status_icon_size, rail_tooltip_enabled, rail_tooltip_metrics,
        resolve_omnibox, resolve_omnibox_with_search, tab_close_center, tab_status_center,
        truncate_title, workspace_form_actions, ChromePalette, ChromeRenderer, ChromeState,
        ChromeTheme, CommandAction, DownloadBadge, DownloadSummary, RailMenuAction, SettingsTab,
        TabDisplayState, TabInfo, WorkspaceInfo, RAIL_STATUS_BADGE_HEIGHT, RAIL_STATUS_HEIGHT,
        RAIL_STATUS_ICON_COLLAPSED, RAIL_STATUS_ICON_EXPANDED, TOOLTIP_HEIGHT_COLLAPSED,
        TOOLTIP_HEIGHT_EXPANDED, TOOLTIP_WIDTH,
    };
    use crate::config::{
        BrowserPreferences, SearchEngine, ThemePreference, WorkspaceColor, WorkspaceIcon,
        DEFAULT_HOME_URL, DEFAULT_SEARCH_URL_PREFIX, RAIL_WIDTH_COLLAPSED, RAIL_WIDTH_EXPANDED,
    };
    use crate::events::{DownloadEngine, DownloadInfo, DownloadState, UiAction};

    fn tab(id: u64, title: &str, url: &str) -> TabInfo {
        TabInfo {
            id,
            workspace_id: 1,
            workspace_name: "Personal".to_string(),
            url: url.to_string(),
            title: title.to_string(),
            is_loading: false,
            display_state: TabDisplayState::Background,
            blocked_requests: 0,
            can_go_back: false,
            can_go_forward: false,
            is_pinned: false,
            favicon: None,
        }
    }

    #[test]
    fn surface_format_prefers_egui_compatible_unorm_formats() {
        use wgpu::TextureFormat as F;

        assert_eq!(
            preferred_surface_format(&[F::Bgra8UnormSrgb, F::Rgba8Unorm]),
            Some(F::Rgba8Unorm)
        );
        assert_eq!(
            preferred_surface_format(&[F::Rgba8UnormSrgb, F::Bgra8Unorm]),
            Some(F::Bgra8Unorm)
        );
    }

    #[test]
    fn surface_format_falls_back_without_panicking() {
        use wgpu::TextureFormat as F;

        assert_eq!(preferred_surface_format(&[]), None);
        assert_eq!(
            preferred_surface_format(&[F::Rgba16Float, F::Bgra8UnormSrgb]),
            Some(F::Rgba16Float)
        );
        assert_eq!(
            preferred_surface_format(&[F::Bgra8UnormSrgb]),
            Some(F::Bgra8UnormSrgb)
        );
    }

    #[test]
    fn resolve_omnibox_passes_through_absolute_urls() {
        assert_eq!(resolve_omnibox("https://github.com"), "https://github.com");
        assert_eq!(resolve_omnibox("http://example.com"), "http://example.com");
        assert_eq!(resolve_omnibox("ftp://example.com"), "ftp://example.com");
        assert_eq!(resolve_omnibox("file:///tmp/a"), "file:///tmp/a");
        assert_eq!(
            resolve_omnibox("data:text/plain,hello"),
            "data:text/plain,hello"
        );
    }

    #[test]
    fn resolve_omnibox_prepends_https_to_bare_domains() {
        assert_eq!(
            resolve_omnibox("github.com/rust-lang"),
            "https://github.com/rust-lang"
        );
        assert_eq!(resolve_omnibox("example.com"), "https://example.com");
        assert_eq!(
            resolve_omnibox("1.2.3.4:8080/path"),
            "https://1.2.3.4:8080/path"
        );
    }

    #[test]
    fn resolve_omnibox_searches_non_url_text() {
        assert_eq!(
            resolve_omnibox("hello world"),
            format!("{DEFAULT_SEARCH_URL_PREFIX}hello+world")
        );
        assert_eq!(
            resolve_omnibox("rust async"),
            format!("{DEFAULT_SEARCH_URL_PREFIX}rust+async")
        );
        // `Url::parse` accepts `localhost:3000` as a custom scheme, so the
        // documented parse-first rule passes it through unchanged.
        assert_eq!(resolve_omnibox("localhost:3000"), "localhost:3000");
        assert!(resolve_omnibox("example.com path").contains("duckduckgo.com"));
    }

    #[test]
    fn resolve_omnibox_encodes_search_query_characters() {
        let resolved = resolve_omnibox("rust & webgpu");
        assert_eq!(
            resolved,
            format!("{DEFAULT_SEARCH_URL_PREFIX}rust+%26+webgpu")
        );

        let unicode = resolve_omnibox("café");
        assert!(unicode.starts_with(DEFAULT_SEARCH_URL_PREFIX));
        assert!(unicode.contains("%C3%A9"));
    }

    #[test]
    fn resolve_omnibox_handles_empty_and_whitespace_input() {
        assert_eq!(resolve_omnibox(""), DEFAULT_HOME_URL);
        assert_eq!(resolve_omnibox("   \t\n"), DEFAULT_HOME_URL);
    }

    #[test]
    fn resolve_omnibox_preserves_view_source_scheme_case_insensitively() {
        assert_eq!(
            resolve_omnibox("view-source:https://example.com"),
            "view-source:https://example.com"
        );
        assert_eq!(
            resolve_omnibox("  VIEW-SOURCE:https://example.com  "),
            "VIEW-SOURCE:https://example.com"
        );
        assert_eq!(resolve_omnibox("view-source:"), "view-source:");
    }

    #[test]
    fn tooltips_are_limited_to_the_collapsed_rail() {
        assert!(rail_tooltip_enabled(false));
        assert!(!rail_tooltip_enabled(true));
    }

    #[test]
    fn collapsed_tooltips_are_compact_translucent_and_render_over_the_page() {
        let (width, height) = rail_tooltip_metrics(false);
        assert_eq!(width, TOOLTIP_WIDTH);
        assert_eq!(height, TOOLTIP_HEIGHT_COLLAPSED);
        assert!((150.0..=220.0).contains(&width));
        assert!((30.0..=48.0).contains(&height));

        for palette in [ChromePalette::DARK, ChromePalette::LIGHT] {
            let [_, _, _, alpha] = palette.tooltip_bg.to_array();
            assert!(alpha < 255, "tooltip background must be translucent");
        }

        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(44.0, 800.0));
        let row = egui::Rect::from_min_size(pos2(3.0, 120.0), vec2(38.0, 44.0));
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let anchor = page_tooltip_anchor(rail, row, screen, width, height);
        assert_eq!(anchor.x, 52.0);
        assert!(anchor.x >= rail.right());
        assert!(anchor.x + width <= screen.right());
        assert!(anchor.y + height <= screen.bottom());
    }

    #[test]
    fn tab_close_button_positions_match_rail_mode() {
        let row = egui::Rect::from_min_size(pos2(10.0, 100.0), vec2(160.0, 36.0));
        assert_eq!(tab_close_center(row, true), pos2(157.0, 118.0));
        assert_eq!(tab_close_center(row, false), pos2(161.0, 109.0));
    }

    #[test]
    fn tab_status_shifts_left_when_close_is_visible() {
        let row = egui::Rect::from_min_size(pos2(10.0, 100.0), vec2(160.0, 36.0));
        let normal = tab_status_center(row, false);
        let shifted = tab_status_center(row, true);
        assert_eq!(normal, pos2(159.0, 118.0));
        assert_eq!(shifted, pos2(140.0, 118.0));
        assert!(shifted.x < normal.x);
    }

    #[test]
    fn rail_address_field_stays_inside_expanded_rail() {
        let ctx = egui::Context::default();
        let palette = ChromePalette::DARK;
        let mut state = ChromeState::default();
        state.omnibox_text =
            "https://example.com/a/very/long/path/that/must/not/push/the/frame/outside".to_string();
        let mut actions = Vec::new();
        let mut field_rect = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0));
        let mut rail_rect = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0));

        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    pos2(0.0, 0.0),
                    vec2(1280.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::SidePanel::left("rail_address_field_test")
                    .exact_width(RAIL_WIDTH_EXPANDED)
                    .resizable(false)
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        let response = ChromeRenderer::build_rail_address_field(
                            ui,
                            ctx,
                            &mut state,
                            &mut actions,
                            &palette,
                        );
                        field_rect = response.rect;
                        rail_rect = ui.clip_rect();
                    });
            },
        );

        assert!(
            field_rect.max.x <= rail_rect.max.x + 0.5,
            "URL frame overflows the rail: {field_rect:?} vs {rail_rect:?}"
        );
        assert!(
            field_rect.width() <= RAIL_WIDTH_EXPANDED + 0.5,
            "URL frame is wider than the rail: {:?}",
            field_rect.width()
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn collapsed_status_icons_scale_up_for_the_narrow_rail() {
        assert_eq!(rail_status_icon_size(true), RAIL_STATUS_ICON_EXPANDED);
        assert_eq!(rail_status_icon_size(false), RAIL_STATUS_ICON_COLLAPSED);
        assert!(RAIL_STATUS_ICON_COLLAPSED > RAIL_STATUS_ICON_EXPANDED);
        assert!(RAIL_STATUS_ICON_COLLAPSED < RAIL_WIDTH_COLLAPSED);
    }

    #[test]
    fn collapsed_status_badges_fit_inside_the_reserved_block() {
        // Collapse/expand, hibernate, Settings, Downloads, shield and RAM rows.
        let used_height = 5.0
            + 30.0
            + 4.0
            + 30.0
            + 4.0
            + 30.0
            + 4.0
            + 30.0
            + 5.0
            + RAIL_STATUS_BADGE_HEIGHT
            + 3.0
            + RAIL_STATUS_BADGE_HEIGHT;
        assert!(used_height <= RAIL_STATUS_HEIGHT);
    }

    #[test]
    fn focus_request_raised_during_render_survives_to_next_frame() {
        assert!(!focus_request_after_render(false, false));
        assert!(focus_request_after_render(false, true));
        assert!(!focus_request_after_render(true, true));
        assert!(!focus_request_after_render(true, false));
    }

    #[test]
    fn palette_click_outside_only_dismisses_on_a_real_click_outside() {
        let rect = egui::Rect::from_min_size(pos2(100.0, 72.0), vec2(560.0, 300.0));
        assert!(command_palette_click_outside(
            rect,
            Some(pos2(50.0, 200.0)),
            true
        ));
        assert!(!command_palette_click_outside(
            rect,
            Some(pos2(200.0, 120.0)),
            true
        ));
        assert!(!command_palette_click_outside(rect, None, true));
        assert!(!command_palette_click_outside(
            rect,
            Some(pos2(50.0, 200.0)),
            false
        ));
    }

    #[test]
    fn page_tooltip_anchor_follows_row_and_stays_over_the_page() {
        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(180.0, 800.0));
        let row = egui::Rect::from_min_size(pos2(6.0, 200.0), vec2(168.0, 36.0));
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let anchor = page_tooltip_anchor(rail, row, screen, TOOLTIP_WIDTH, TOOLTIP_HEIGHT_EXPANDED);
        assert_eq!(
            anchor,
            pos2(188.0, row.center().y - TOOLTIP_HEIGHT_EXPANDED * 0.5)
        );
        assert_eq!(anchor.y + TOOLTIP_HEIGHT_EXPANDED * 0.5, row.center().y);
        assert!(anchor.x >= rail.right());
        assert!(anchor.x + TOOLTIP_WIDTH <= screen.right());
        assert!(anchor.y + TOOLTIP_HEIGHT_EXPANDED <= screen.bottom());
    }

    #[test]
    fn page_tooltip_anchor_clamps_near_screen_bottom() {
        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(180.0, 800.0));
        let row = egui::Rect::from_min_size(pos2(6.0, 790.0), vec2(168.0, 36.0));
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let anchor = page_tooltip_anchor(rail, row, screen, TOOLTIP_WIDTH, TOOLTIP_HEIGHT_EXPANDED);
        assert_eq!(anchor.y, 800.0 - TOOLTIP_HEIGHT_EXPANDED - 6.0);
        assert!(anchor.y + TOOLTIP_HEIGHT_EXPANDED <= screen.bottom());
    }

    #[test]
    fn advance_command_selection_handles_keys_and_edges() {
        assert_eq!(advance_command_selection(0, 0, false, false), 0);
        assert_eq!(advance_command_selection(7, 0, true, true), 0);
        assert_eq!(advance_command_selection(0, 5, false, false), 0);
        assert_eq!(advance_command_selection(2, 5, false, true), 3);
        assert_eq!(advance_command_selection(4, 5, false, true), 4);
        assert_eq!(advance_command_selection(0, 5, true, false), 4);
        assert_eq!(advance_command_selection(2, 5, true, false), 1);
        assert_eq!(advance_command_selection(2, 5, true, true), 2);
        assert_eq!(advance_command_selection(99, 5, false, true), 4);
    }

    #[test]
    fn palette_row_fill_prefers_keyboard_selection_over_hover() {
        let palette = &ChromePalette::DARK;
        assert_eq!(
            palette_row_fill(palette, true, true),
            palette.palette_row_selected
        );
        assert_eq!(
            palette_row_fill(palette, true, false),
            palette.palette_row_selected
        );
        assert_eq!(
            palette_row_fill(palette, false, true),
            palette.palette_row_hover
        );
        assert_eq!(
            palette_row_fill(palette, false, false),
            Color32::TRANSPARENT
        );
    }

    #[test]
    fn theme_maps_system_appearance_and_defaults_to_dark() {
        use winit::window::Theme;

        assert_eq!(ChromeTheme::default(), ChromeTheme::Dark);
        assert_eq!(ChromeTheme::from_winit(None), ChromeTheme::Dark);
        assert_eq!(
            ChromeTheme::from_winit(Some(Theme::Dark)),
            ChromeTheme::Dark
        );
        assert_eq!(
            ChromeTheme::from_winit(Some(Theme::Light)),
            ChromeTheme::Light
        );
    }

    #[test]
    fn light_palette_keeps_contrast_and_differs_from_dark() {
        let dark = &ChromePalette::DARK;
        let light = &ChromePalette::LIGHT;

        assert_ne!(dark.rail_bg, light.rail_bg);
        assert_ne!(dark.text, light.text);
        assert_ne!(dark.palette_bg, light.palette_bg);
        assert_ne!(dark.tooltip_bg, light.tooltip_bg);

        // The light rail must be brighter than its text, while the dark rail
        // is darker than its text. This catches inverted palette entries.
        let lum = |color: Color32| {
            0.2126 * color.r() as f32 + 0.7152 * color.g() as f32 + 0.0722 * color.b() as f32
        };
        assert!(lum(light.rail_bg) > lum(light.text));
        assert!(lum(dark.rail_bg) < lum(dark.text));

        // Selected rows need a visible fill in both themes.
        assert_ne!(light.palette_row_selected, Color32::TRANSPARENT);
        assert_ne!(light.palette_row_hover, Color32::TRANSPARENT);
    }

    #[test]
    fn workspace_color_tints_native_chrome_accents() {
        let mut state = ChromeState::default();
        let before = state.palette().rail_accent;
        state.workspaces = vec![WorkspaceInfo {
            id: 7,
            name: "Studio".to_string(),
            icon: WorkspaceIcon::Palette,
            color: WorkspaceColor::Green,
            is_default: true,
            is_active: true,
        }];
        state.active_workspace_id = 7;

        let tinted = state.palette();
        assert_ne!(tinted.rail_accent, before);
        assert_ne!(tinted.rail_row_active, ChromePalette::DARK.rail_row_active);
        assert_eq!(tinted.rail_bg, ChromePalette::DARK.rail_bg);
    }

    #[test]
    fn truncate_title_counts_unicode_scalar_values() {
        assert_eq!(truncate_title("short", 24), "short");
        assert_eq!(
            truncate_title("123456789012345678901234", 24),
            "123456789012345678901234"
        );
        assert_eq!(
            truncate_title("1234567890123456789012345", 24),
            "123456789012345678901234…"
        );
        assert_eq!(truncate_title("日本語タイトル", 4), "日本語タ…");
        assert_eq!(truncate_title("abc", 0), "…");
    }

    #[test]
    fn clamp_command_selection_handles_empty_and_out_of_range_values() {
        assert_eq!(clamp_command_selection(10, 0), 0);
        assert_eq!(clamp_command_selection(0, 0), 0);
        assert_eq!(clamp_command_selection(3, 5), 3);
        assert_eq!(clamp_command_selection(9, 5), 4);
    }

    #[test]
    fn empty_palette_offers_actions_then_open_tabs() {
        let prefs = BrowserPreferences::default();
        let items = command_palette_items(
            "",
            &[tab(1, "Rust", "https://rust-lang.org")],
            Some(1),
            &prefs,
        );
        assert_eq!(items[0].action, CommandAction::NewTab(None));
        assert_eq!(items[1].action, CommandAction::TogglePinTab(1));
        assert_eq!(items[2].action, CommandAction::HibernateAll);
        assert_eq!(items[3].action, CommandAction::ToggleRail);
        assert_eq!(items[4].action, CommandAction::OpenSettings);
        assert_eq!(items[5].action, CommandAction::OpenDownloads);
        assert_eq!(items[6].action, CommandAction::SelectTab(1));
    }

    #[test]
    fn palette_finds_the_downloads_bubble() {
        let items = command_palette_items("download", &[], None, &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::OpenDownloads));
    }

    #[test]
    fn download_labels_and_sizes_are_native_readable() {
        assert_eq!(
            download_state_label(DownloadState::InProgress),
            "Downloading"
        );
        assert_eq!(download_state_label(DownloadState::Completed), "Completed");
        assert_eq!(format_download_bytes(0), "0 B");
        assert_eq!(format_download_bytes(-5), "0 B");
        assert_eq!(format_download_bytes(512), "512 B");
        assert_eq!(format_download_bytes(2048), "2.0 KB");
        assert_eq!(format_download_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(
            download_engine_label(DownloadEngine::Parallel, 8),
            "Parallel · 8 connections"
        );
        assert_eq!(download_engine_label(DownloadEngine::Parallel, 1), "Native");
        assert_eq!(download_engine_label(DownloadEngine::Cef, 1), "CEF");
    }

    #[test]
    fn download_eta_formats_remaining_time() {
        assert_eq!(download_eta_label(0, 1_000, 100), "ETA 10s");
        assert_eq!(download_eta_label(0, 120_000, 1_000), "ETA 2m 0s");
        assert_eq!(download_eta_label(0, 7_200_000, 1_000), "ETA 2h 0m");
    }

    #[test]
    fn download_eta_is_hidden_without_usable_speed_or_remaining_bytes() {
        assert_eq!(download_eta_label(0, 1_000, 0), "");
        assert_eq!(download_eta_label(0, 0, 100), "");
        assert_eq!(download_eta_label(1_000, 1_000, 100), "");
    }

    #[test]
    fn palette_filters_tabs_case_insensitively() {
        let tabs = [
            tab(1, "Rust Language", "https://rust-lang.org"),
            tab(2, "Docs", "https://docs.rs"),
        ];
        let items = command_palette_items("RUST", &tabs, Some(1), &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::SelectTab(1)));
        assert!(!items
            .iter()
            .any(|item| item.action == CommandAction::SelectTab(2)));
    }

    #[test]
    fn palette_offers_navigation_and_new_tab_for_queries() {
        let items =
            command_palette_items("hello world", &[], Some(7), &BrowserPreferences::default());
        assert_eq!(items[0].label, "Search in this tab");
        assert_eq!(
            items[0].action,
            CommandAction::Navigate {
                tab_id: 7,
                url: format!("{DEFAULT_SEARCH_URL_PREFIX}hello+world"),
            }
        );
        assert_eq!(items[1].label, "Search in a new tab");
        assert_eq!(
            items[1].action,
            CommandAction::NewTab(Some(format!("{DEFAULT_SEARCH_URL_PREFIX}hello+world")))
        );
    }

    #[test]
    fn palette_labels_address_navigation_for_this_and_new_tabs() {
        let items =
            command_palette_items("example.com", &[], Some(7), &BrowserPreferences::default());
        assert_eq!(items[0].label, "Open in this tab");
        assert_eq!(
            items[0].action,
            CommandAction::Navigate {
                tab_id: 7,
                url: "https://example.com".to_string(),
            }
        );
        assert_eq!(items[1].label, "Open in a new tab");
        assert_eq!(
            items[1].action,
            CommandAction::NewTab(Some("https://example.com".to_string()))
        );
    }

    #[test]
    fn palette_offers_devtools_action() {
        let items = command_palette_items("devtools", &[], Some(7), &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::OpenDevTools));

        let mut state = ChromeState::default();
        state.active_tab_id = Some(7);
        state.command_palette_open = true;
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::OpenDevTools, &mut state, &mut actions);
        assert_eq!(actions, vec![UiAction::OpenDevTools { tab_id: 7 }]);
        assert!(!state.command_palette_open);
    }

    #[test]
    fn default_state_shows_the_workspace_rail() {
        let state = ChromeState::default();
        assert!(state.rail_expanded);
        assert!(!state.command_palette_open);
        assert_eq!(state.command_selection, 0);
    }

    #[test]
    fn loading_spinner_rotates_and_spaces_its_segments() {
        let start = loading_spinner_angle(0.0, 0, 12);
        assert_eq!(start, 0.0);

        let next_segment = loading_spinner_angle(0.0, 1, 12);
        assert!((next_segment - start - std::f32::consts::TAU / 12.0).abs() < 0.0001);

        let later = loading_spinner_angle(0.5, 0, 12);
        assert!(later > start);
        assert!((later - std::f32::consts::TAU * 0.55).abs() < 0.0001);
    }

    #[test]
    fn loading_spinner_points_stay_on_its_circle() {
        let center = pos2(10.0, 20.0);
        let radius = 8.0;
        let point = loading_spinner_point(center, radius, 0.0);
        assert!((point.x - 18.0).abs() < 0.0001);
        assert!((point.y - 20.0).abs() < 0.0001);

        let point = loading_spinner_point(center, radius, std::f32::consts::FRAC_PI_2);
        assert!((point.x - 10.0).abs() < 0.0001);
        assert!((point.y - 28.0).abs() < 0.0001);
    }

    #[test]
    fn state_open_palette_seeds_query_and_close_resets_it() {
        let mut state = ChromeState::default();
        state.omnibox_text = "https://example.com".to_string();
        state.open_command_palette();
        assert!(state.command_palette_open);
        assert_eq!(state.command_palette_query, "https://example.com");
        assert!(state.command_palette_focus_requested);
        assert!(!state.command_palette_select_all_requested);

        state.close_command_palette();
        assert!(!state.command_palette_open);
        assert!(state.command_palette_query.is_empty());
        assert!(!state.command_palette_focus_requested);
        assert!(!state.command_palette_select_all_requested);
    }

    #[test]
    fn palette_text_selection_is_deferred_until_input_has_focus() {
        let mut state = ChromeState::default();
        state.omnibox_text = "https://duckduckgo.com".to_string();
        state.open_command_palette();

        // First overlay frame requests focus; the selection must wait.
        let focus_before = state.command_palette_focus_requested;
        let select_all_before = state.command_palette_select_all_requested;
        assert!(focus_before);
        assert!(!select_all_before);

        state.command_palette_focus_requested =
            focus_request_after_render(focus_before, state.command_palette_focus_requested);
        state.command_palette_select_all_requested = focus_before;
        assert!(!state.command_palette_focus_requested);
        assert!(state.command_palette_select_all_requested);

        // Second overlay frame consumes the selection request.
        state.command_palette_select_all_requested = false;
        assert!(!state.command_palette_select_all_requested);
    }

    #[test]
    fn theme_preference_overrides_live_system_theme() {
        assert_eq!(
            effective_theme(ThemePreference::System, ChromeTheme::Light),
            ChromeTheme::Light
        );
        assert_eq!(
            effective_theme(ThemePreference::Dark, ChromeTheme::Light),
            ChromeTheme::Dark
        );
        assert_eq!(
            effective_theme(ThemePreference::Light, ChromeTheme::Dark),
            ChromeTheme::Light
        );

        let mut state = ChromeState::default();
        state.set_system_theme(ChromeTheme::Light);
        assert_eq!(state.theme, ChromeTheme::Light);

        state.set_preferences(BrowserPreferences {
            theme: ThemePreference::Dark,
            ..BrowserPreferences::default()
        });
        assert_eq!(state.theme, ChromeTheme::Dark);
        state.set_system_theme(ChromeTheme::Light);
        assert_eq!(state.theme, ChromeTheme::Dark);

        state.set_preferences(BrowserPreferences {
            theme: ThemePreference::System,
            ..BrowserPreferences::default()
        });
        assert_eq!(state.theme, ChromeTheme::Light);
    }

    #[test]
    fn preferences_do_not_override_a_manual_rail_toggle() {
        let mut state = ChromeState::default();
        state.rail_expanded = false;
        state.set_preferences(BrowserPreferences {
            rail_expanded: true,
            search_engine: SearchEngine::Google,
            ..BrowserPreferences::default()
        });

        assert!(!state.rail_expanded);
        assert_eq!(state.preferences.search_engine, SearchEngine::Google);
    }

    #[test]
    fn rail_toggle_emits_a_persistable_action() {
        let mut state = ChromeState::default();
        let initial = state.rail_expanded;
        let mut actions = Vec::new();
        state.toggle_rail(&mut actions);

        assert_eq!(state.rail_expanded, !initial);
        assert!(
            matches!(actions.as_slice(), [UiAction::SetRailExpanded(expanded)] if *expanded == state.rail_expanded)
        );
    }

    #[test]
    fn settings_overlay_is_exclusive_and_dismissable() {
        let mut state = ChromeState::default();
        state.omnibox_text = "https://example.com".to_string();
        state.open_command_palette();
        state.open_tab_menu(1, pos2(190.0, 200.0));
        state.open_settings();

        assert!(state.settings_open);
        assert!(state.has_overlay());
        assert!(!state.command_palette_open);
        assert!(state.menu.is_none());
        assert!(!state.command_palette_focus_requested);
        assert_eq!(state.settings_draft, state.preferences);

        state.close_settings();
        assert!(!state.has_overlay());
    }

    #[test]
    fn settings_overlay_opens_on_the_requested_page() {
        let mut state = ChromeState::default();
        assert_eq!(state.settings_tab, SettingsTab::General);

        state.open_settings_tab(SettingsTab::Workspaces);
        assert!(state.settings_open);
        assert_eq!(state.settings_tab, SettingsTab::Workspaces);

        state.close_settings();
        state.open_settings();
        assert_eq!(state.settings_tab, SettingsTab::General);
    }

    fn bubble_download(id: u32, state: DownloadState) -> DownloadInfo {
        DownloadInfo {
            id,
            tab_id: 1,
            url: format!("https://example.com/file-{id}.zip"),
            suggested_name: format!("file-{id}.zip"),
            full_path: format!("/Users/test/Downloads/file-{id}.zip"),
            received_bytes: 25 * i64::from(id),
            total_bytes: 100,
            percent_complete: (25 * id) as i32,
            current_speed: 4096,
            state,
            engine: DownloadEngine::Parallel,
            segments: 8,
            temp_path: format!("/Users/test/Downloads/file-{id}.zip.kasumi.part"),
            started_at: 100,
            updated_at: 100,
        }
    }

    #[test]
    fn downloads_bubble_renders_multiple_downloads_without_invalid_layout() {
        let ctx = egui::Context::default();
        let mut state = ChromeState::default();
        state.open_downloads();
        state
            .downloads
            .push(bubble_download(1, DownloadState::InProgress));
        state
            .downloads
            .push(bubble_download(2, DownloadState::Paused));
        state
            .downloads
            .push(bubble_download(3, DownloadState::Completed));
        let mut actions = Vec::new();

        // egui caches an Area's measured size. Render several frames so a widget
        // that reports an infinite width cannot poison the next frame's centering
        // math with a NaN max rect.
        for _ in 0..3 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        pos2(0.0, 0.0),
                        vec2(1280.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ctx| ChromeRenderer::build_downloads_bubble(ctx, &mut state, &mut actions),
            );
            if let Some(area) =
                egui::AreaState::load(&ctx, egui::Id::new("kasumi_downloads_bubble"))
            {
                let size = area.size.unwrap_or_default();
                assert!(
                    size.x.is_finite() && size.y.is_finite(),
                    "Downloads bubble area size must stay finite: {size:?}"
                );
            }
        }

        assert!(actions.is_empty());
        assert!(state.downloads_open);
    }

    #[test]
    fn download_summary_aggregates_multiple_downloads() {
        let downloads = vec![
            bubble_download(1, DownloadState::InProgress),
            bubble_download(2, DownloadState::Paused),
            bubble_download(3, DownloadState::Completed),
            bubble_download(4, DownloadState::Canceled),
        ];
        let summary = DownloadSummary::from_downloads(&downloads);

        assert_eq!(summary.active, 2);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        // Canceled rows are excluded from the aggregate byte totals.
        assert_eq!(summary.received, 25 + 50 + 75);
        assert_eq!(summary.total, 300);
        // Only the in-progress row contributes speed.
        assert_eq!(summary.speed, 4096);
        assert_eq!(summary.status_text(), "2 active · 1 done · 1 failed");

        let mut unknown = bubble_download(1, DownloadState::InProgress);
        unknown.total_bytes = 0;
        assert_eq!(DownloadSummary::from_downloads(&[unknown]).percent(), 0);
    }

    #[test]
    fn finished_download_rows_drop_the_progress_bar() {
        assert!(download_row_uses_progress_bar(DownloadState::InProgress));
        assert!(download_row_uses_progress_bar(DownloadState::Paused));
        assert!(!download_row_uses_progress_bar(DownloadState::Completed));
        assert!(!download_row_uses_progress_bar(DownloadState::Canceled));
        assert!(!download_row_uses_progress_bar(DownloadState::Interrupted));
    }

    #[test]
    fn download_rail_badge_tracks_active_transfers_and_history() {
        assert_eq!(download_rail_badge(&[]), DownloadBadge::Hidden);

        let history = vec![bubble_download(1, DownloadState::Completed)];
        assert_eq!(download_rail_badge(&history), DownloadBadge::History);

        let active = vec![
            bubble_download(1, DownloadState::InProgress),
            bubble_download(2, DownloadState::Paused),
            bubble_download(3, DownloadState::Completed),
        ];
        assert_eq!(download_rail_badge(&active), DownloadBadge::Active(2));
    }

    #[test]
    fn downloads_bubble_state_is_exclusive_and_dismissable() {
        let mut state = ChromeState::default();

        state.open_downloads();
        assert!(state.downloads_open);
        assert!(state.has_overlay());
        assert!(!state.settings_open);
        assert!(!state.command_palette_open);

        state.open_command_palette();
        assert!(!state.downloads_open);
        assert!(state.command_palette_open);

        state.open_downloads();
        state.open_settings();
        assert!(!state.downloads_open);
        assert!(state.settings_open);

        state.open_downloads();
        state.open_tab_menu(1, pos2(80.0, 120.0));
        assert!(!state.downloads_open);
        assert!(state.menu.is_some());

        state.open_downloads();
        state.close_active_overlay();
        assert!(!state.downloads_open);
        assert!(!state.has_overlay());

        state.toggle_downloads();
        assert!(state.downloads_open);
        state.toggle_downloads();
        assert!(!state.downloads_open);
    }

    #[test]
    fn downloads_bubble_anchor_stays_beside_rail_and_inside_the_window() {
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let size = downloads_bubble_size(screen);
        assert!(size.x.is_finite() && size.y.is_finite());

        let expanded = downloads_bubble_anchor(screen, 180.0);
        assert!(expanded.x >= 180.0);
        assert!(expanded.y >= screen.top());
        assert!(expanded.y + size.y <= screen.bottom());

        let collapsed = downloads_bubble_anchor(screen, 44.0);
        assert!(collapsed.x < expanded.x);
        assert!(collapsed.x >= 44.0);

        let small = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(220.0, 180.0));
        let small_size = downloads_bubble_size(small);
        let anchor = downloads_bubble_anchor(small, 180.0);
        assert!(anchor.x >= small.left());
        assert!(anchor.y >= small.top());
        assert!(anchor.x + small_size.x <= small.right());
        assert!(anchor.y + small_size.y <= small.bottom());
    }

    #[test]
    fn workspace_forms_are_separate_when_creating() {
        let workspace = WorkspaceInfo {
            id: 1,
            name: "Personal".to_string(),
            icon: WorkspaceIcon::Work,
            color: WorkspaceColor::Blue,
            is_default: true,
            is_active: true,
        };
        let actions = workspace_form_actions(
            Some(&workspace),
            "asd",
            WorkspaceIcon::Palette,
            WorkspaceColor::Green,
            false,
            "Studio",
            WorkspaceIcon::School,
            WorkspaceColor::Purple,
            true,
            false,
        );

        assert_eq!(
            actions,
            vec![UiAction::CreateWorkspace {
                name: "Studio".to_string(),
                icon: WorkspaceIcon::School,
                color: WorkspaceColor::Purple,
            }]
        );
    }

    #[test]
    fn workspace_edit_requires_an_explicit_save() {
        let workspace = WorkspaceInfo {
            id: 1,
            name: "Personal".to_string(),
            icon: WorkspaceIcon::Work,
            color: WorkspaceColor::Blue,
            is_default: true,
            is_active: true,
        };
        assert!(workspace_form_actions(
            Some(&workspace),
            "asd",
            WorkspaceIcon::Palette,
            WorkspaceColor::Green,
            false,
            "New workspace",
            WorkspaceIcon::Work,
            WorkspaceColor::Blue,
            false,
            false,
        )
        .is_empty());

        let actions = workspace_form_actions(
            Some(&workspace),
            "asd",
            WorkspaceIcon::Palette,
            WorkspaceColor::Green,
            true,
            "New workspace",
            WorkspaceIcon::Work,
            WorkspaceColor::Blue,
            false,
            false,
        );
        assert_eq!(
            actions,
            vec![UiAction::UpdateWorkspace {
                workspace_id: 1,
                name: "asd".to_string(),
                icon: WorkspaceIcon::Palette,
                color: WorkspaceColor::Green,
            }]
        );
    }

    #[test]
    fn settings_seeds_separate_workspace_forms() {
        let mut state = ChromeState::default();
        state.workspaces = vec![
            WorkspaceInfo {
                id: 1,
                name: "Personal".to_string(),
                icon: WorkspaceIcon::Work,
                color: WorkspaceColor::Blue,
                is_default: true,
                is_active: false,
            },
            WorkspaceInfo {
                id: 2,
                name: "Studio".to_string(),
                icon: WorkspaceIcon::Palette,
                color: WorkspaceColor::Purple,
                is_default: false,
                is_active: true,
            },
        ];
        state.active_workspace_id = 2;

        state.open_settings_tab(SettingsTab::Workspaces);
        assert_eq!(state.selected_workspace_id, Some(2));
        assert_eq!(state.workspace_edit_name_draft, "Studio");
        assert_eq!(state.workspace_new_name_draft, "New workspace");

        state.select_workspace_for_editing(1);
        assert_eq!(state.selected_workspace_id, Some(1));
        assert_eq!(state.workspace_edit_name_draft, "Personal");
        assert_eq!(state.workspace_edit_color_draft, WorkspaceColor::Blue);
        assert_eq!(state.workspace_new_name_draft, "New workspace");
    }

    #[test]
    fn workspace_creator_is_a_separate_settings_mode() {
        let mut state = ChromeState::default();
        state.open_settings();
        assert!(!state.workspace_create_mode);

        state.open_workspace_creator();
        assert!(state.settings_open);
        assert_eq!(state.settings_tab, SettingsTab::Workspaces);
        assert!(state.workspace_create_mode);

        state.workspace_new_name_draft = "Half-finished".to_string();
        state.close_workspace_creator();
        assert!(!state.workspace_create_mode);
        assert_eq!(state.workspace_new_name_draft, "New workspace");
    }

    #[test]
    fn closing_settings_resets_the_workspace_creator() {
        let mut state = ChromeState::default();
        state.open_workspace_creator();
        state.workspace_new_name_draft = "Half-finished".to_string();

        state.close_settings();
        assert!(!state.settings_open);
        assert!(!state.workspace_create_mode);
        assert_eq!(state.workspace_new_name_draft, "New workspace");
    }

    #[test]
    fn following_active_workspace_clears_the_stale_editor_selection() {
        let mut state = ChromeState::default();
        state.selected_workspace_id = Some(7);
        state.workspace_edit_name_draft = "Stale".to_string();
        state.workspace_edit_icon_draft = WorkspaceIcon::Palette;
        state.workspace_edit_color_draft = WorkspaceColor::Purple;

        state.follow_active_workspace();
        assert_eq!(state.selected_workspace_id, None);
        assert!(state.workspace_edit_name_draft.is_empty());
        assert_eq!(state.workspace_edit_icon_draft, WorkspaceIcon::default());
        assert_eq!(state.workspace_edit_color_draft, WorkspaceColor::default());
    }

    #[test]
    fn command_palette_uses_selected_search_provider() {
        let prefs = BrowserPreferences {
            search_engine: SearchEngine::Bing,
            ..BrowserPreferences::default()
        };
        let items = command_palette_items("hello world", &[], Some(7), &prefs);
        assert_eq!(
            items[0].action,
            CommandAction::Navigate {
                tab_id: 7,
                url: "https://www.bing.com/search?q=hello+world".to_string(),
            }
        );
        assert_eq!(
            resolve_omnibox_with_search("rust async", prefs.search_url_prefix()),
            "https://www.bing.com/search?q=rust+async"
        );
    }

    #[test]
    fn command_palette_open_settings_action_opens_native_overlay() {
        let mut state = ChromeState::default();
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::OpenSettings, &mut state, &mut actions);

        assert!(state.settings_open);
        assert!(actions.is_empty());
    }

    #[test]
    fn tab_menu_metrics_and_anchor_stay_beside_rail() {
        let (width, height) = rail_menu_metrics();
        assert_eq!(width, 224.0);
        assert_eq!(height, 10.0 * 28.0 + 14.0);

        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(180.0, 800.0));
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let anchor = rail_menu_anchor(rail, pos2(400.0, 300.0), screen, width, height);
        assert_eq!(anchor, pos2(400.0, 292.0));
        assert!(anchor.x >= rail.right());
        assert!(anchor.y + height <= screen.bottom());

        let near_bottom = rail_menu_anchor(rail, pos2(400.0, 795.0), screen, width, height);
        assert_eq!(near_bottom.y, 800.0 - height - 6.0);
        assert!(near_bottom.y + height <= screen.bottom());
    }

    #[test]
    fn tab_menu_click_outside_ignores_missing_pointer_and_inside_clicks() {
        let card = egui::Rect::from_min_size(pos2(188.0, 92.0), vec2(224.0, 294.0));
        assert!(rail_menu_click_outside(
            card,
            Some(pos2(100.0, 200.0)),
            true
        ));
        assert!(!rail_menu_click_outside(
            card,
            Some(pos2(200.0, 120.0)),
            true
        ));
        assert!(!rail_menu_click_outside(
            card,
            Some(pos2(188.0, 92.0)),
            true
        ));
        assert!(!rail_menu_click_outside(card, None, true));
        assert!(!rail_menu_click_outside(
            card,
            Some(pos2(100.0, 200.0)),
            false
        ));
    }

    #[test]
    fn tab_menu_rows_enable_only_available_commands() {
        let mut state = ChromeState::default();
        state.tabs = vec![tab(1, "First", "https://first.example")];

        let items = rail_menu_items(&state, 1);
        assert_eq!(items.len(), 10);
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::NewTab && item.enabled));
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::TogglePinTab && item.enabled));
        assert_eq!(
            items
                .iter()
                .find(|item| item.action == RailMenuAction::TogglePinTab)
                .map(|item| item.label),
            Some("Pin tab")
        );
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::CopyUrl && item.enabled));
        assert!(!items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseOtherTabs && item.enabled));
        assert!(!items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseTabsToRight && item.enabled));
        assert_eq!(
            items
                .iter()
                .find(|item| item.action == RailMenuAction::CloseTabsToRight)
                .map(|item| item.label),
            Some("Close tabs below this tab")
        );
        assert!(!items
            .iter()
            .any(|item| item.action == RailMenuAction::ReopenClosedTab && item.enabled));

        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::OpenDevTools && item.enabled));

        state.tabs.push(tab(2, "Second", "https://second.example"));
        state.can_reopen_tab = true;
        let items = rail_menu_items(&state, 1);
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseOtherTabs && item.enabled));
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseTabsToRight && item.enabled));
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::ReopenClosedTab && item.enabled));

        let last_items = rail_menu_items(&state, 2);
        assert!(!last_items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseTabsToRight && item.enabled));
    }

    #[test]
    fn tab_menu_offers_unpin_for_pinned_tabs() {
        let mut state = ChromeState::default();
        let mut pinned = tab(1, "Pinned", "https://pinned.example");
        pinned.is_pinned = true;
        state.tabs = vec![pinned];

        let items = rail_menu_items(&state, 1);
        assert_eq!(
            items
                .iter()
                .find(|item| item.action == RailMenuAction::TogglePinTab)
                .map(|item| item.label),
            Some("Unpin tab")
        );
    }

    #[test]
    fn palette_pin_action_toggles_the_active_tab() {
        let mut state = ChromeState::default();
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::TogglePinTab(7), &mut state, &mut actions);

        assert_eq!(actions, vec![UiAction::TogglePinTab { tab_id: 7 }]);
        assert!(!state.command_palette_open);
    }

    #[test]
    fn palette_offer_pin_action_for_pin_query() {
        let items = command_palette_items(
            "pin",
            &[tab(1, "Rust", "https://rust-lang.org")],
            Some(1),
            &BrowserPreferences::default(),
        );
        assert!(items.iter().any(|item| {
            item.action == CommandAction::TogglePinTab(1) && item.label == "Pin active tab"
        }));
    }

    #[test]
    fn tab_menu_actions_map_to_tab_manager_commands() {
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::NewTab, 3),
            Some(UiAction::NewTab { url: None })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::DuplicateTab, 3),
            Some(UiAction::DuplicateTab { tab_id: 3 })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::TogglePinTab, 3),
            Some(UiAction::TogglePinTab { tab_id: 3 })
        );
        assert_eq!(rail_menu_ui_action(RailMenuAction::CopyUrl, 3), None);
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::Reload, 3),
            Some(UiAction::Reload {
                tab_id: 3,
                ignore_cache: false
            })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::CloseTab, 3),
            Some(UiAction::CloseTab { tab_id: 3 })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::CloseOtherTabs, 3),
            Some(UiAction::CloseOtherTabs { tab_id: 3 })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::CloseTabsToRight, 3),
            Some(UiAction::CloseTabsToRight { tab_id: 3 })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::ReopenClosedTab, 3),
            Some(UiAction::ReopenClosedTab)
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::OpenDevTools, 3),
            Some(UiAction::OpenDevTools { tab_id: 3 })
        );
    }

    #[test]
    fn opening_tab_menu_replaces_palette_and_reset_selection() {
        let mut state = ChromeState::default();
        state.omnibox_text = "https://example.com".to_string();
        state.command_selection = 4;
        state.menu_selection = 5;
        state.open_command_palette();
        state.open_tab_menu(9, pos2(200.0, 100.0));

        assert!(!state.command_palette_open);
        assert!(state.command_palette_query.is_empty());
        assert!(!state.command_palette_focus_requested);
        let menu = state.menu.as_ref().expect("tab menu should be open");
        assert_eq!(menu.tab_id, 9);
        assert_eq!(menu.anchor, pos2(200.0, 100.0));
        assert_eq!(state.menu_selection, 0);

        state.close_tab_menu();
        assert!(state.menu.is_none());
        assert_eq!(state.menu_selection, 0);
    }
}
