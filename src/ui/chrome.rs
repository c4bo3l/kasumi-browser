#![allow(deprecated)]
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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    pos2, vec2, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Layout, Order,
    Response, RichText, Sense, Stroke, TextEdit, TextureOptions, Ui, UiBuilder,
};
use egui_wgpu::ScreenDescriptor;

use crate::config::{
    rail_width, BrowserPreferences, SearchEngine, ThemePreference, Workspace, WorkspaceColor,
    WorkspaceIcon, WorkspaceStartupBehavior, COMMAND_PALETTE_TOP_OFFSET, COMMAND_PALETTE_WIDTH,
    DEFAULT_SEARCH_URL_PREFIX, HISTORY_VISIBLE_PAGE_SIZE, MAX_IDLE_TIMEOUT_MINUTES,
    MIN_IDLE_TIMEOUT_MINUTES, PAGE_HUD_HEIGHT, PAGE_HUD_MARGIN,
};
use crate::events::{
    DownloadEngine, DownloadInfo, DownloadNotification, DownloadNotificationKind, DownloadState,
    FindOptions, FindSnippet, PrintOptions, PrintPaperSize, UiAction, WebNotification,
};
use crate::storage::db::HistoryItem;
use crate::storage::import::{BrowserProfile, BrowserSource, ImportReport};

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

/// Tooltips wrap inside this maximum card width; short labels get a narrower
/// card sized from the laid-out text.
const TOOLTIP_MAX_WIDTH: f32 = 280.0;
const TOOLTIP_MIN_WIDTH: f32 = 120.0;
const TOOLTIP_TEXT_MARGIN_X: f32 = 8.0;
const TOOLTIP_TEXT_MARGIN_Y: f32 = 6.0;
const TOOLTIP_FONT_SIZE: f32 = 11.0;
const TOOLTIP_MAX_ROWS: usize = 6;
const TOOLTIP_HEIGHT_EXPANDED: f32 = 44.0;
const TOOLTIP_HEIGHT_COLLAPSED: f32 = 38.0;
const RAIL_MENU_WIDTH: f32 = 224.0;
const RAIL_MENU_ROW_HEIGHT: f32 = 28.0;
const RAIL_MENU_PADDING: f32 = 7.0;
const RAIL_STATUS_HEIGHT: f32 = 144.0;
const RAIL_STATUS_ICON_EXPANDED: f32 = 15.0;
const RAIL_STATUS_ICON_COLLAPSED: f32 = 20.0;
const RAIL_STATUS_BADGE_HEIGHT: f32 = 24.0;
/// Compact bottom-HUD controls sized to fit `PAGE_HUD_HEIGHT` without clipping.
const PAGE_HUD_BUTTON_SIZE: f32 = 18.0;
const PAGE_HUD_ICON_INSET: f32 = 2.0;
const PAGE_HUD_VERTICAL_MARGIN: i8 = 2;
const SETTINGS_WIDTH: f32 = 660.0;
const SETTINGS_MAX_CONTENT_HEIGHT: f32 = 560.0;
const DOWNLOADS_WIDTH: f32 = 460.0;
const DOWNLOADS_MAX_HEIGHT: f32 = 460.0;
const DOWNLOADS_BOTTOM_MARGIN: f32 = 18.0;
const DOWNLOADS_RAIL_GAP: f32 = 12.0;
const PERMISSION_PROMPT_WIDTH: f32 = 420.0;
const PERMISSION_PROMPT_TOP_OFFSET: f32 = 72.0;
const PRINT_DIALOG_WIDTH: f32 = 440.0;
const DISPLAY_PICKER_WIDTH: f32 = 540.0;
const DISPLAY_PICKER_TOP_OFFSET: f32 = 64.0;
const DISPLAY_PICKER_MAX_HEIGHT: f32 = 560.0;

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
    Private,
    Globe,
    DisplayScreen,
    DisplayWindow,
    Close,
    Pin,
    PinOff,
    Copy,
    Sidebar,
    Settings,
    Download,
    Upload,
    Print,
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
    Notifications,
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
    /// Native find result count, or -1 when this tab has not been searched.
    pub find_count: i32,
    /// One-based index of the active match, or -1 when unknown.
    pub find_active_ordinal: i32,
    /// Query is waiting for this hibernated tab to wake before it can run.
    pub find_pending: bool,
    /// Rich context snippets emitted by the renderer find shim.
    pub find_snippets: Vec<FindSnippet>,
    /// Invalid-regex or shim error for this tab's current find query.
    pub find_error: String,
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
    NewPrivateWindow,
    Navigate { tab_id: u64, url: String },
    TogglePinTab(u64),
    HibernateAll,
    ToggleRail,
    OpenDevTools,
    Print,
    OpenSettings,
    OpenBrowserImport,
    ExportBookmarks,
    ExportHistory,
    OpenDownloads,
    OpenHistory,
    BookmarkActiveTab,
    OpenBookmarkManager,
}

/// One row in the floating command palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPaletteItem {
    pub icon: &'static str,
    pub label: String,
    pub subtitle: String,
    pub action: CommandAction,
}

/// Visual tone of a transient native notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
}

/// A short-lived notification painted in the transparent overlay above the CEF
/// page. It never captures input and disappears automatically.
#[derive(Debug, Clone)]
pub struct ChromeToast {
    pub kind: ToastKind,
    pub title: String,
    pub message: String,
    pub created_at: Instant,
    pub duration: Duration,
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

/// Which native permission request the shared prompt is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPromptKind {
    Media,
    Notifications,
}

/// A native camera/microphone or web-notification prompt mirrored from
/// `TabManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub kind: PermissionPromptKind,
    pub request_id: u64,
    pub tab_id: u64,
    pub origin: String,
    pub permissions: u32,
    pub label: String,
    pub private: bool,
}

/// A native Web Notification shown in a small always-on-top window. It is
/// separate from the click-through download toasts so the card can receive a
/// click and dispatch the page's `notification.onclick` handler.
#[derive(Debug, Clone)]
pub struct WebNotificationToast {
    pub notification: WebNotification,
    pub created_at: Instant,
    pub duration: Duration,
}

/// One display or window Kasumi can pass to Chromium's `getDisplayMedia`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMediaSource {
    pub device_id: String,
    pub label: String,
    pub is_window: bool,
}

/// Native source picker mirrored from `TabManager` into the overlay renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMediaPicker {
    pub tab_id: u64,
    pub token: String,
    pub sources: Vec<DisplayMediaSource>,
}

/// Native Linux print panel mirrored from `TabManager`. It is painted in the
/// transparent overlay so the page stays visible and no web view is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintDialogState {
    pub tab_id: u64,
    pub has_selection: bool,
    pub options: PrintOptions,
    pub printers: Vec<String>,
    pub default_printer: String,
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
    /// Disabled section heading for the workspace move rows.
    MoveTabToHeader,
    /// Move the right-clicked unpinned tab into another workspace.
    MoveTabToWorkspace {
        workspace_id: u64,
    },
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
    Import,
}

/// One row in the rail tab context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailMenuItem {
    pub label: String,
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
    /// Recent visited pages mirrored from SQLite for the native history manager.
    pub history: Vec<HistoryItem>,
    /// True while the standalone history manager owns the shared overlay.
    pub history_open: bool,
    /// In-memory filter applied to `history` by the native manager.
    pub history_query: String,
    /// Number of matching rows currently laid out; additional rows are shown
    /// in pages so a large snapshot never blocks the winit frame loop.
    pub history_visible_count: usize,
    /// Second-click guard for the destructive Clear history action.
    pub history_clear_confirm: bool,
    /// Native bookmark manager search and single-row editor drafts.
    pub bookmark_search_query: String,
    pub selected_bookmark_key: Option<(u64, String)>,
    pub bookmark_title_draft: String,
    pub bookmark_url_draft: String,
    pub bookmark_workspace_id_draft: u64,
    pub bookmark_folder_draft: String,
    /// Transient notification shown above the CEF child view.
    pub toast: Option<ChromeToast>,
    /// Notifications held until no modal overlay owns the shared surface. The
    /// display timer starts only when a toast becomes visible.
    pub pending_toasts: VecDeque<ChromeToast>,
    /// Current website notification shown in its own clickable native window.
    pub web_notification: Option<WebNotificationToast>,
    /// Website notifications waiting for the modal overlay and the current
    /// notification to clear. Timers start when a notification becomes visible.
    pub pending_web_notifications: VecDeque<WebNotificationToast>,
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
    /// Set when a keyboard shortcut asks the chrome to copy the active URL.
    /// egui owns the clipboard, so the request is served by the next chrome
    /// frame instead of the shortcut handler.
    pub copy_url_requested: bool,
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
    /// Highest-priority modal: a CEF WebRTC permission request awaiting a reply.
    pub permission_prompt: Option<PermissionPrompt>,
    /// Native display/window picker for a `getDisplayMedia` request.
    pub display_media_picker: Option<DisplayMediaPicker>,
    /// Linux print panel waiting for a user decision.
    pub print_dialog: Option<PrintDialogState>,
    /// Highlighted row in the display picker.
    pub display_media_selection: usize,
    /// True when at least one closed tab can be restored with Cmd/Ctrl+Z.
    pub can_reopen_tab: bool,
    /// Current OS light/dark appearance. The user preference may override it.
    pub system_theme: ChromeTheme,
    /// Effective appearance used by every native chrome surface.
    pub theme: ChromeTheme,
    /// Persisted user preferences mirrored into the chrome state.
    pub preferences: BrowserPreferences,
    /// True while the native find-in-page bar is visible in the overlay.
    pub find_bar_open: bool,
    /// Current query in the find bar.
    pub find_query: String,
    /// Case-sensitive find toggle.
    pub find_match_case: bool,
    /// Regex toggle. Advanced mode is handled by the JS highlighter.
    pub find_regex: bool,
    /// Whole-word toggle. Advanced mode is handled by the JS highlighter.
    pub find_whole_word: bool,
    /// Multi-term mode (`error | timeout`). Each term gets its own colour.
    pub find_multi_term: bool,
    /// `This tab` vs `All tabs` search scope.
    pub find_all_tabs: bool,
    /// Snippet drawer expansion state.
    pub find_drawer_open: bool,
    /// Set when the find input should take focus on the next overlay frame.
    pub find_focus_requested: bool,
    /// Set when another overlay superseded find and CEF highlights need clearing.
    pub find_dismiss_requested: bool,
    /// Match count for the active tab, mirrored from CEF.
    pub find_count: i32,
    /// One-based active match index, mirrored from CEF.
    pub find_active_ordinal: i32,
    /// Active tab's query is waiting for its browser to wake.
    pub find_pending: bool,
    /// Error emitted by the rich shim, e.g. an invalid regular expression.
    pub find_error: String,
    /// Show only snippets matching this substring in the drawer.
    pub find_filter_enabled: bool,
    pub find_filter_query: String,
    /// True while the native Settings overlay is visible.
    pub settings_open: bool,
    /// Page currently shown in the Settings overlay.
    pub settings_tab: SettingsTab,
    /// True while the standalone bookmark manager dialog owns the overlay.
    pub bookmark_manager_open: bool,
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
    /// Local browser profiles discovered off the UI thread.
    pub browser_import_profiles: Vec<BrowserProfile>,
    /// True while profile discovery is running.
    pub browser_import_detecting: bool,
    /// Profile currently selected in the Import settings page.
    pub browser_import_selected_source: Option<BrowserSource>,
    pub browser_import_bookmarks: bool,
    pub browser_import_history: bool,
    /// True while a parsed batch is being applied by the SQLite worker.
    pub browser_import_running: bool,
    /// Latest worker stage message shown in the Import page.
    pub browser_import_progress: String,
    /// Outcome of the most recent import attempt.
    pub browser_import_report: Option<ImportReport>,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab_id: None,
            workspaces: Vec::new(),
            active_workspace_id: 1,
            bookmarks: Vec::new(),
            history: Vec::new(),
            history_open: false,
            history_query: String::new(),
            history_visible_count: HISTORY_VISIBLE_PAGE_SIZE,
            history_clear_confirm: false,
            bookmark_search_query: String::new(),
            selected_bookmark_key: None,
            bookmark_title_draft: String::new(),
            bookmark_url_draft: String::new(),
            bookmark_workspace_id_draft: 1,
            bookmark_folder_draft: String::new(),
            toast: None,
            pending_toasts: VecDeque::new(),
            web_notification: None,
            pending_web_notifications: VecDeque::new(),
            downloads: Vec::new(),
            omnibox_text: String::new(),
            omnibox_focused: false,
            last_synced_tab_id: None,
            last_synced_url: String::new(),
            copy_url_requested: false,
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
            permission_prompt: None,
            display_media_picker: None,
            print_dialog: None,
            display_media_selection: 0,
            can_reopen_tab: false,
            find_bar_open: false,
            find_query: String::new(),
            find_match_case: false,
            find_regex: false,
            find_whole_word: false,
            find_multi_term: false,
            find_all_tabs: false,
            find_drawer_open: false,
            find_focus_requested: false,
            find_dismiss_requested: false,
            find_count: -1,
            find_active_ordinal: -1,
            find_pending: false,
            find_error: String::new(),
            find_filter_enabled: false,
            find_filter_query: String::new(),
            system_theme: ChromeTheme::default(),
            theme: ChromeTheme::default(),
            preferences: BrowserPreferences::default(),
            settings_open: false,
            settings_tab: SettingsTab::General,
            bookmark_manager_open: false,
            settings_draft: BrowserPreferences::default(),
            selected_workspace_id: None,
            workspace_edit_name_draft: String::new(),
            workspace_edit_icon_draft: WorkspaceIcon::default(),
            workspace_edit_color_draft: WorkspaceColor::default(),
            workspace_create_mode: false,
            workspace_new_name_draft: "New workspace".to_string(),
            workspace_new_icon_draft: WorkspaceIcon::default(),
            workspace_new_color_draft: WorkspaceColor::default(),
            browser_import_profiles: Vec::new(),
            browser_import_detecting: false,
            browser_import_selected_source: None,
            browser_import_bookmarks: true,
            browser_import_history: true,
            browser_import_running: false,
            browser_import_progress: String::new(),
            browser_import_report: None,
        }
    }
}

impl ChromeState {
    /// Show a short native notification above the page. Notifications raised
    /// while a modal owns the overlay are queued instead of expiring unseen.
    pub fn show_toast(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        message: impl Into<String>,
    ) {
        let toast = ChromeToast {
            kind,
            title: title.into(),
            message: message.into(),
            created_at: Instant::now(),
            duration: Duration::from_secs(4),
        };
        if self.has_modal_overlay() || self.toast.is_some() {
            const MAX_PENDING_TOASTS: usize = 4;
            self.pending_toasts.push_back(toast);
            while self.pending_toasts.len() > MAX_PENDING_TOASTS {
                self.pending_toasts.pop_front();
            }
            return;
        }
        self.toast = Some(toast);
    }

    /// Promote the oldest queued notification once the overlay is free.
    pub fn promote_pending_toast(&mut self) {
        if self.toast.is_some() || self.has_modal_overlay() {
            return;
        }
        if let Some(mut toast) = self.pending_toasts.pop_front() {
            // Start the visibility timer now, not when the event arrived.
            toast.created_at = Instant::now();
            self.toast = Some(toast);
        }
    }

    /// Ask the next chrome frame to copy the active page URL. Keyboard
    /// shortcuts reach the shell without an egui context, so the clipboard
    /// write is deferred to the renderer that owns the `egui::Context`.
    pub fn request_copy_url(&mut self) {
        self.copy_url_requested = true;
    }

    /// URL a copy command should place on the clipboard. The active tab is
    /// authoritative; the mirrored omnibox text covers the window before the
    /// first tab exists.
    pub fn active_url(&self) -> Option<String> {
        let url = self
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == self.active_tab_id)
            .map(|tab| tab.url.as_str())
            .unwrap_or(self.omnibox_text.as_str());
        (!url.is_empty()).then(|| url.to_string())
    }

    /// Surface a terminal download outcome as a native notification.
    pub fn show_download_notification(&mut self, notification: DownloadNotification) {
        let name = if notification.file_name.trim().is_empty() {
            "download".to_string()
        } else {
            notification.file_name
        };
        match notification.kind {
            DownloadNotificationKind::Completed => self.show_toast(
                ToastKind::Success,
                "Download complete",
                format!("{name} is ready"),
            ),
            DownloadNotificationKind::Failed => self.show_toast(
                ToastKind::Error,
                "Download failed",
                format!("Could not finish {name}"),
            ),
        }
    }

    /// Queue or show a website notification. Real notifications stay visible
    /// longer than download toasts, and `requireInteraction` keeps them until
    /// the user dismisses or clicks them.
    pub fn show_web_notification(&mut self, notification: WebNotification) {
        // A non-empty tag replaces the previous notification from the same tab.
        if !notification.tag.is_empty() {
            let same_slot = |toast: &WebNotificationToast| {
                toast.notification.tab_id == notification.tab_id
                    && toast.notification.tag == notification.tag
            };
            if self.web_notification.as_ref().is_some_and(same_slot) {
                self.web_notification = None;
            }
            self.pending_web_notifications
                .retain(|toast| !same_slot(toast));
        }
        let duration = if notification.require_interaction {
            // `requireInteraction` notifications stay until the user acts.
            Duration::from_secs(60 * 60)
        } else {
            Duration::from_secs(9)
        };
        let toast = WebNotificationToast {
            notification,
            created_at: Instant::now(),
            duration,
        };
        if self.has_modal_overlay() || self.web_notification.is_some() {
            const MAX_PENDING_WEB_NOTIFICATIONS: usize = 4;
            self.pending_web_notifications.push_back(toast);
            while self.pending_web_notifications.len() > MAX_PENDING_WEB_NOTIFICATIONS {
                self.pending_web_notifications.pop_front();
            }
            return;
        }
        self.web_notification = Some(toast);
    }

    /// Promote the oldest queued website notification once the modal overlay
    /// and the visible notification have cleared.
    pub fn promote_pending_web_notification(&mut self) {
        if self.web_notification.is_some() || self.has_modal_overlay() {
            return;
        }
        if let Some(mut toast) = self.pending_web_notifications.pop_front() {
            toast.created_at = Instant::now();
            self.web_notification = Some(toast);
        }
    }

    /// Drop a website notification once its display window has elapsed and
    /// return it so the shell can dispatch the page's `close` handler.
    pub fn expire_web_notification(&mut self) -> Option<WebNotification> {
        let expired = self
            .web_notification
            .as_ref()
            .is_some_and(|toast| toast.created_at.elapsed() >= toast.duration);
        if !expired {
            return None;
        }
        let notification = self.web_notification.take().map(|toast| toast.notification);
        self.promote_pending_web_notification();
        notification
    }

    /// Remove a website notification by id, whether it is visible or queued.
    /// Page-initiated `notification.close()` calls land here.
    pub fn remove_web_notification(&mut self, id: &str) {
        if self
            .web_notification
            .as_ref()
            .is_some_and(|toast| toast.notification.id == id)
        {
            self.web_notification = None;
            self.promote_pending_web_notification();
        }
        self.pending_web_notifications
            .retain(|toast| toast.notification.id != id);
    }

    /// Take the visible website notification so the shell can hide its window
    /// before dispatching the click/close event.
    pub fn take_web_notification(&mut self) -> Option<WebNotification> {
        let notification = self
            .web_notification
            .take()
            .map(|toast| toast.notification)?;
        self.promote_pending_web_notification();
        Some(notification)
    }

    /// Drop a toast once its display window has elapsed.
    pub fn expire_toast(&mut self) {
        let expired = self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.created_at.elapsed() >= toast.duration);
        if expired {
            self.toast = None;
            self.promote_pending_toast();
        }
    }

    /// True when the shared overlay must capture pointer/keyboard input.
    pub fn has_modal_overlay(&self) -> bool {
        self.permission_prompt.is_some()
            || self.print_dialog.is_some()
            || self.display_media_picker.is_some()
            || self.find_bar_open
            || self.command_palette_open
            || self.menu.is_some()
            || self.settings_open
            || self.bookmark_manager_open
            || self.history_open
            || self.downloads_open
    }

    /// True when only a click-through notification is visible.
    pub fn is_toast_only(&self) -> bool {
        self.toast.is_some() && !self.has_modal_overlay()
    }

    fn reset_bookmark_editor(&mut self) {
        self.bookmark_search_query.clear();
        self.selected_bookmark_key = None;
        self.bookmark_title_draft.clear();
        self.bookmark_url_draft.clear();
        self.bookmark_workspace_id_draft = self.active_workspace_id;
        self.bookmark_folder_draft.clear();
    }

    /// Open the find bar and focus its input on the next overlay frame.
    pub fn open_find_bar(&mut self) {
        self.find_bar_open = true;
        self.find_focus_requested = true;
        // Find and the modal palette never share the overlay at once.
        self.close_command_palette();
        self.close_tab_menu();
        self.settings_open = false;
        self.bookmark_manager_open = false;
        self.history_open = false;
        self.downloads_open = false;
    }

    /// Hide the find bar and clear its transient query state.
    pub fn close_find_bar(&mut self) {
        self.find_bar_open = false;
        self.find_query.clear();
        self.find_drawer_open = false;
        self.find_focus_requested = false;
        self.find_count = -1;
        self.find_active_ordinal = -1;
        self.find_pending = false;
        self.find_error.clear();
        self.find_filter_enabled = false;
        self.find_filter_query.clear();
    }

    /// Copy the active tab's latest CEF find result into the find bar.
    pub fn sync_find_result(&mut self, tab_id: u64) {
        if Some(tab_id) != self.active_tab_id {
            return;
        }
        if let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) {
            self.find_count = tab.find_count;
            self.find_active_ordinal = tab.find_active_ordinal;
            self.find_pending = tab.find_pending;
            self.find_error = tab.find_error.clone();
        }
    }

    /// Another modal surface is taking over the shared overlay. Close find and
    /// defer highlight cleanup to the next frame, where actions can be emitted.
    fn dismiss_find_for_overlay(&mut self) {
        if self.find_bar_open {
            self.close_find_bar();
            self.find_dismiss_requested = true;
        }
    }

    /// Snapshot the current find toggles for the action bridge.
    pub fn find_options(&self) -> FindOptions {
        FindOptions {
            regex: self.find_regex,
            whole_word: self.find_whole_word,
            multi_term: self.find_multi_term,
            all_tabs: self.find_all_tabs,
        }
    }

    /// Open the command palette seeded with the active tab's current address.
    pub fn open_command_palette(&mut self) {
        self.dismiss_find_for_overlay();
        if !self.command_palette_open {
            self.command_palette_query = self.omnibox_text.clone();
            self.command_selection = 0;
            self.command_palette_focus_requested = true;
            self.command_palette_select_all_requested = false;
        }
        self.command_palette_open = true;
        self.close_tab_menu();
        self.settings_open = false;
        self.bookmark_manager_open = false;
        self.history_open = false;
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
        self.dismiss_find_for_overlay();
        // The palette and menu share one transparent overlay window and must
        // never both claim it. Clearing the palette here also avoids a stale
        // focus request when the menu is dismissed.
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.bookmark_manager_open = false;

        let (width, height) = rail_menu_metrics(self, tab_id);
        self.menu = Some(RailMenu {
            tab_id,
            anchor,
            width,
            height,
        });
        self.menu_selection = 0;
        self.tooltip = None;
        self.settings_open = false;
        self.history_open = false;
        self.downloads_open = false;
    }

    pub fn close_tab_menu(&mut self) {
        self.menu = None;
        self.menu_selection = 0;
    }

    /// Open the native Settings overlay on the General page.
    pub fn open_settings(&mut self) {
        self.dismiss_find_for_overlay();
        self.open_settings_tab(SettingsTab::General);
    }

    /// Open Settings directly on the browser-import page.
    pub fn open_browser_import(&mut self) {
        self.dismiss_find_for_overlay();
        self.open_settings_tab(SettingsTab::Import);
    }

    /// Install discovered profiles and keep the selection valid when a refresh
    /// removes a previously selected browser.
    pub fn set_browser_profiles(&mut self, profiles: Vec<BrowserProfile>) {
        self.browser_import_profiles = profiles;
        self.browser_import_detecting = false;
        self.browser_import_progress.clear();
        if let Some(source) = self.browser_import_selected_source {
            if !self
                .browser_import_profiles
                .iter()
                .any(|profile| profile.source == source)
            {
                self.browser_import_selected_source = None;
            }
        }
    }

    pub fn set_browser_import_progress(&mut self, message: String) {
        self.browser_import_progress = message;
    }

    pub fn set_browser_import_report(&mut self, report: ImportReport) {
        self.browser_import_running = false;
        self.browser_import_progress.clear();
        self.browser_import_report = Some(report);
    }

    pub fn reset_browser_import_result(&mut self) {
        self.browser_import_running = false;
        self.browser_import_progress.clear();
        self.browser_import_report = None;
    }

    /// Open the Arc/Edge-style downloads bubble in the shared overlay.
    pub fn open_downloads(&mut self) {
        self.dismiss_find_for_overlay();
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.close_tab_menu();
        self.tooltip = None;
        self.settings_open = false;
        self.bookmark_manager_open = false;
        self.history_open = false;
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

    /// Emit a transient dismissal for the highest-priority modal. The actual
    /// CEF callback is resolved by `TabManager`; clearing state here alone would
    /// leave the request hanging.
    pub fn dismiss_permission_prompt(&self, actions: &mut Vec<UiAction>) {
        if let Some(prompt) = self.permission_prompt.as_ref() {
            match prompt.kind {
                PermissionPromptKind::Media => {
                    actions.push(UiAction::ResolveMediaAccessPermission {
                        request_id: prompt.request_id,
                        granted: false,
                        persist: false,
                    });
                }
                PermissionPromptKind::Notifications => {
                    actions.push(UiAction::ResolveNotificationPermission {
                        request_id: prompt.request_id,
                        granted: false,
                        persist: false,
                    });
                }
            }
        }
    }

    /// Reject a display-source request without persisting anything.
    pub fn dismiss_display_media_picker(&self, actions: &mut Vec<UiAction>) {
        if let Some(picker) = self.display_media_picker.as_ref() {
            actions.push(UiAction::ResolveDisplayMediaPicker {
                token: picker.token.clone(),
                device_id: None,
            });
        }
    }

    /// Cancel the native print panel without sending a job to CUPS.
    pub fn dismiss_print_dialog(&self, actions: &mut Vec<UiAction>) {
        if let Some(dialog) = self.print_dialog.as_ref() {
            actions.push(UiAction::RespondPrintDialog {
                tab_id: dialog.tab_id,
                accept: false,
                options: dialog.options.clone(),
            });
        }
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
        self.bookmark_manager_open = false;
        self.history_open = false;
        self.downloads_open = false;
        self.settings_tab = tab;
        self.settings_draft = self.preferences.clone();
        self.workspace_create_mode = false;
        self.reset_workspace_create_draft();
        self.sync_workspace_draft();
        self.reset_bookmark_editor();
        self.settings_open = true;
    }

    /// Open Workspaces directly in the dedicated creation form. The rail's
    /// **New workspace** button uses this so creation is never mixed into the
    /// list of existing workspaces.
    pub fn open_workspace_creator(&mut self) {
        self.dismiss_find_for_overlay();
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
        self.reset_bookmark_editor();
    }

    /// Open the standalone bookmark manager dialog on the shared native
    /// overlay. It has its own shortcut and never requires opening Settings.
    pub fn open_bookmark_manager(&mut self) {
        self.dismiss_find_for_overlay();
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.close_tab_menu();
        self.tooltip = None;
        self.settings_open = false;
        self.history_open = false;
        self.downloads_open = false;
        self.reset_bookmark_editor();
        self.bookmark_manager_open = true;
    }

    pub fn close_bookmark_manager(&mut self) {
        self.bookmark_manager_open = false;
        self.reset_bookmark_editor();
    }

    /// Open the standalone history manager on the shared native overlay.
    pub fn open_history(&mut self) {
        self.history_clear_confirm = false;
        self.history_visible_count = HISTORY_VISIBLE_PAGE_SIZE;
        self.dismiss_find_for_overlay();
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.command_palette_select_all_requested = false;
        self.close_tab_menu();
        self.tooltip = None;
        self.settings_open = false;
        self.bookmark_manager_open = false;
        self.downloads_open = false;
        self.history_open = true;
    }

    pub fn toggle_history(&mut self) {
        if self.history_open {
            self.close_history();
        } else {
            self.open_history();
        }
    }

    pub fn close_history(&mut self) {
        self.history_open = false;
        self.history_query.clear();
        self.history_visible_count = HISTORY_VISIBLE_PAGE_SIZE;
        self.history_clear_confirm = false;
        self.tooltip = None;
    }

    pub fn set_history(&mut self, items: Vec<HistoryItem>) {
        self.history = items;
        self.history_visible_count = HISTORY_VISIBLE_PAGE_SIZE;
    }

    /// Reveal one additional page of history rows from the in-memory snapshot.
    pub fn show_more_history(&mut self) {
        self.history_visible_count = self
            .history_visible_count
            .saturating_add(HISTORY_VISIBLE_PAGE_SIZE);
    }

    /// Close whichever shared overlay is currently visible.
    pub fn close_active_overlay(&mut self) {
        if self.settings_open {
            self.close_settings();
        } else if self.bookmark_manager_open {
            self.close_bookmark_manager();
        } else if self.history_open {
            self.close_history();
        } else if self.menu.is_some() {
            self.close_tab_menu();
        } else if self.command_palette_open {
            self.close_command_palette();
        } else if self.downloads_open {
            self.close_downloads();
        } else if self.find_bar_open {
            self.close_find_bar();
        }
    }

    pub fn has_overlay(&self) -> bool {
        self.has_modal_overlay() || self.toast.is_some()
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
        let editable = |workspace: &&WorkspaceInfo| !Workspace::is_private_id(workspace.id);
        let selected = self
            .workspaces
            .iter()
            .filter(editable)
            .find(|workspace| Some(workspace.id) == self.selected_workspace_id)
            .or_else(|| {
                self.workspaces
                    .iter()
                    .filter(editable)
                    .find(|workspace| workspace.id == self.active_workspace_id)
            })
            .or_else(|| self.workspaces.iter().find(editable))
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
// Surface acquisition
// ---------------------------------------------------------------------------

/// Acquire the next swapchain image, treating the non-fatal wgpu statuses as
/// ordinary frame skips instead of errors.
///
/// - `Occluded` is expected on macOS and Windows whenever the window is fully
///   hidden (minimized, behind another app, or on an inactive Space). The shell
///   requests a redraw every loop iteration, so logging each skip would flood
///   the log at display refresh rate. It is logged at `trace` instead, and the
///   chrome resumes on the first frame after the window server exposes it again.
/// - `Outdated` and `Lost` are recoverable by reconfiguring the surface, which
///   previously only happened on `Resized` and could leave a window blank.
/// - `Timeout` simply means the swapchain is not ready; try again next tick.
///
/// Returns `None` when the frame must be skipped. Callers keep their UI state
/// (and the actions egui produced) intact either way.
fn acquire_surface_frame(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    surface_config: &wgpu::SurfaceConfiguration,
    label: &str,
) -> Option<wgpu::SurfaceTexture> {
    match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(frame)
        | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => Some(frame),
        wgpu::CurrentSurfaceTexture::Occluded => {
            tracing::trace!("{label}: surface occluded; skipping frame");
            None
        }
        wgpu::CurrentSurfaceTexture::Timeout => {
            tracing::trace!("{label}: surface acquire timed out; skipping frame");
            None
        }
        wgpu::CurrentSurfaceTexture::Outdated => {
            tracing::debug!("{label}: surface outdated; reconfiguring");
            surface.configure(device, surface_config);
            None
        }
        wgpu::CurrentSurfaceTexture::Lost => {
            // The swapchain is gone (display change, GPU switch, wake from
            // sleep). Reconfiguring is the documented recovery for a surface
            // that is still bound to a live window; if that is not enough the
            // next acquire reports `Validation` and the log below fires.
            tracing::debug!("{label}: surface lost; reconfiguring");
            surface.configure(device, surface_config);
            None
        }
        wgpu::CurrentSurfaceTexture::Validation => {
            tracing::warn!("{label}: surface validation error; skipping frame");
            None
        }
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
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let egui_ctx = egui::Context::default();
        // Register the Material Symbols fallback used by every rail icon. It
        // is pinned to the egui 0.30-compatible crate release.
        register_material_icon_font(&egui_ctx);
        // Tooltips must appear immediately; the default delay makes the chrome
        // controls look unlabeled during quick hovers.
        egui_ctx.all_styles_mut(|style| {
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
        let mut full_output = egui_ctx.run_ui(raw_input, |ui| {
            self.build_ui(ui, state, &mut actions);
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        for (id, deltas) in full_output.textures_delta.set.drain() {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let Some(output_frame) =
            acquire_surface_frame(&self.surface, &self.device, &self.surface_config, "chrome")
        else {
            return actions;
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
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(state.theme.clear_color()),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in full_output.textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(output_frame);

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

    fn build_ui(&self, ui: &mut Ui, state: &mut ChromeState, actions: &mut Vec<UiAction>) {
        let ctx = ui.ctx().clone();
        Self::serve_copy_url_request(&ctx, state);
        self.build_rail(ui, state, actions);
        Self::build_page_hud(&ctx, state, actions);

        // The CEF view occupies everything to the right of the rail. The
        // central panel is transparent so it never paints over page content.
        egui::CentralPanel::no_frame()
            .frame(Frame::NONE.fill(Color32::TRANSPARENT))
            .show(ui, |_ui| {});
    }

    /// Serve a Cmd/Ctrl+Shift+C request raised by a shell or CEF shortcut. The
    /// clipboard write happens here because egui owns the platform clipboard
    /// and flushes it as part of this frame's platform output.
    fn serve_copy_url_request(ctx: &egui::Context, state: &mut ChromeState) {
        if !state.copy_url_requested {
            return;
        }
        state.copy_url_requested = false;
        let Some(url) = state.active_url() else {
            tracing::debug!("Copy URL shortcut ignored: no active page");
            return;
        };
        ctx.copy_text(url);
        state.show_toast(ToastKind::Success, "Copied", "Link copied to clipboard");
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
                    let (rect, response) =
                        ui.allocate_exact_size(vec2(34.0, 34.0), egui::Sense::click());
                    let (r, g, b) = workspace.color.rgb();
                    let accent = Color32::from_rgb(r, g, b);
                    let fill = if active {
                        mix_color(palette.rail_surface, accent, 0.42)
                    } else if response.hovered() {
                        palette.rail_row_hover
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
                    ui.painter().rect_stroke(
                        rect.shrink(0.5),
                        CornerRadius::same(9),
                        Stroke::new(
                            if active { 2.0_f32 } else { 1.0_f32 },
                            if active {
                                accent
                            } else {
                                palette.palette_border
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                    let icon = if Workspace::is_private_id(workspace.id) {
                        RailIcon::Private
                    } else {
                        workspace.icon.into()
                    };
                    paint_rail_icon(
                        ui,
                        rect.shrink(9.0),
                        icon,
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
                    let tooltip = if Workspace::is_private_id(workspace.id) {
                        "Incognito — private space, never saved".to_string()
                    } else if active {
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
            let (rect, response) =
                ui.allocate_exact_size(vec2(width - 12.0, 26.0), egui::Sense::click());
            let (r, g, b) = workspace.color.rgb();
            let color = Color32::from_rgb(r, g, b);
            let fill = if active {
                mix_color(palette.rail_surface, color, 0.34)
            } else if response.hovered() {
                palette.rail_row_hover
            } else {
                Color32::TRANSPARENT
            };
            ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
            if active {
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    CornerRadius::same(7),
                    Stroke::new(1.0_f32, color),
                    egui::StrokeKind::Inside,
                );
            }
            let icon_rect = egui::Rect::from_center_size(
                pos2(rect.min.x + 16.0, rect.center().y),
                vec2(16.0, 16.0),
            );
            let icon = if Workspace::is_private_id(workspace.id) {
                RailIcon::Private
            } else {
                workspace.icon.into()
            };
            paint_rail_icon(ui, icon_rect, icon, color);
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
        Frame::NONE
            .fill(palette.rail_surface)
            .stroke(Stroke::new(1.0_f32, palette.palette_border))
            .corner_radius(CornerRadius::same(9))
            .outer_margin(egui::Margin::symmetric(6, 0))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Explicit spaces below define the gaps; egui's implicit
                    // item spacing would otherwise add 8 px between each glyph.
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (icon_rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                    paint_rail_icon(ui, icon_rect, RailIcon::Globe, palette.rail_accent);
                    ui.add_space(4.0);

                    // Each 22 px action slot includes its 4 px trailing gap.
                    // The scroll area keeps TextEdit's hidden-text allocation
                    // from expanding the frame, even for very long URLs.
                    let action_slot_width = 22.0;
                    let input_width = (ui.available_width() - action_slot_width * 2.0).max(24.0);
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
                                    .frame(Frame::NONE),
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

                    let (bookmark_rect, bookmark_response) =
                        ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::click());
                    let bookmark_key = active_bookmark_key(state);
                    let is_bookmarked = bookmark_key.is_some();
                    paint_bookmark_toggle(
                        ui,
                        egui::Rect::from_center_size(bookmark_rect.center(), vec2(12.0, 12.0)),
                        is_bookmarked,
                        if is_bookmarked {
                            palette.rail_accent
                        } else {
                            palette.text
                        },
                    );
                    let private_active = active_tab_is_private(state);
                    if bookmark_response.clicked() {
                        if private_active {
                            state.show_toast(
                                ToastKind::Error,
                                "Incognito",
                                "Bookmarks are unavailable in a private space",
                            );
                        } else if let Some((workspace_id, url)) = bookmark_key.clone() {
                            actions.push(UiAction::DeleteBookmark { workspace_id, url });
                            state.show_toast(
                                ToastKind::Success,
                                "Bookmark removed",
                                "Removed from this workspace",
                            );
                        } else if let Some(tab_id) = state.active_tab_id {
                            actions.push(UiAction::AddBookmark { tab_id });
                            state.show_toast(
                                ToastKind::Success,
                                "Bookmark saved",
                                "Added to this workspace",
                            );
                        }
                    }
                    bookmark_response.on_hover_text(if private_active {
                        "Bookmarks are unavailable in Incognito"
                    } else if is_bookmarked {
                        "Remove bookmark"
                    } else {
                        "Bookmark this page"
                    });

                    let (copy_rect, copy_response) =
                        ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::click());
                    paint_rail_icon(
                        ui,
                        egui::Rect::from_center_size(copy_rect.center(), vec2(12.0, 12.0)),
                        RailIcon::Copy,
                        palette.text,
                    );
                    if copy_response.clicked() {
                        ctx.copy_text(state.omnibox_text.clone());
                        state.show_toast(ToastKind::Success, "Copied", "Link copied to clipboard");
                    }
                    copy_response.on_hover_text("Copy URL");
                });
            })
            .response
    }

    /// Draw the bottom page HUD inside the strip reserved below the CEF view.
    /// Hover tooltips are handed to the native overlay window so the page can
    /// never cover them.
    pub fn build_page_hud(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        let rail = rail_width(state.rail_expanded);
        let screen = ctx.viewport_rect();
        let height = PAGE_HUD_HEIGHT;
        let margin = PAGE_HUD_MARGIN;
        let rect = egui::Rect::from_min_max(
            pos2(rail + margin, screen.bottom() - height - margin),
            pos2(screen.right() - margin, screen.bottom() - margin),
        );
        let active_tab_id = state.active_tab_id;
        let blocked_requests = state
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == active_tab_id)
            .map_or(0, |tab| tab.blocked_requests);
        let ram_mb = state.ram_mb;
        let palette = state.palette();
        let set_tooltip = |state: &mut ChromeState, response: &Response, text: String| {
            if response.hovered() {
                let (width, height) = tooltip_size_for_text(ctx, &text, false);
                state.tooltip = Some(RailTooltip {
                    text,
                    anchor: bottom_hud_tooltip_anchor(response.rect, screen, width, height),
                    width,
                    height,
                });
            }
        };

        egui::Area::new(egui::Id::new("kasumi_page_hud"))
            .order(egui::Order::Middle)
            .fixed_pos(rect.min)
            // Seed the sizing pass with the real strip height. Without this,
            // egui constrains the first frame using its 600x400 default area
            // size and briefly clamps the HUD toward the middle of the window.
            .default_size(vec2(rect.width(), height))
            .show(ctx, |ui| {
                ui.set_width(rect.width());
                Frame::NONE
                    .fill(palette.menu_bg)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(8, PAGE_HUD_VERTICAL_MARGIN))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let command = Self::rail_icon_button_with_inset(
                                ui,
                                vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                RailIcon::Search,
                                &palette,
                                PAGE_HUD_ICON_INSET,
                            );
                            if command.clicked() {
                                state.open_command_palette();
                            }
                            set_tooltip(
                                state,
                                &command,
                                "Command palette (Cmd/Ctrl+K)".to_string(),
                            );

                            let devtools = Self::rail_icon_button_with_inset(
                                ui,
                                vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                RailIcon::Code,
                                &palette,
                                PAGE_HUD_ICON_INSET,
                            );
                            if devtools.clicked() {
                                if let Some(tab_id) = active_tab_id {
                                    actions.push(UiAction::OpenDevTools { tab_id });
                                }
                            }
                            set_tooltip(state, &devtools, "DevTools (F12)".to_string());

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let ram = Self::rail_icon_button_with_inset(
                                    ui,
                                    vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                    RailIcon::Chip,
                                    &palette,
                                    PAGE_HUD_ICON_INSET,
                                );
                                set_tooltip(state, &ram, format!("RAM {ram_mb} MB"));
                                ui.label(
                                    RichText::new(format!("{ram_mb}"))
                                        .size(9.0)
                                        .color(palette.text_dim),
                                );

                                let shield = Self::rail_icon_button_with_inset(
                                    ui,
                                    vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                    RailIcon::Shield,
                                    &palette,
                                    PAGE_HUD_ICON_INSET,
                                );
                                set_tooltip(
                                    state,
                                    &shield,
                                    format!("{blocked_requests} ads blocked"),
                                );
                                ui.label(
                                    RichText::new(blocked_requests.to_string())
                                        .size(9.0)
                                        .color(palette.shield_ok),
                                );
                            });
                        });
                    });
            });
    }

    /// Draw the collapsible left workspace rail.
    fn build_rail(&self, root_ui: &mut Ui, state: &mut ChromeState, actions: &mut Vec<UiAction>) {
        // Tooltips are rendered in a transparent overlay above the page, so
        // collect the hovered row for this frame instead of painting in-rail.
        let mut rail_tooltip: Option<RailTooltip> = None;
        let mut menu_request: Option<(u64, egui::Pos2)> = None;
        // Pass a borrow into the panel closure so the collected tooltip can be
        // read again after the closure finishes.
        let tooltip_slot = &mut rail_tooltip;
        let ctx = root_ui.ctx().clone();
        let width = rail_width(state.rail_expanded);
        let palette = state.palette();
        let palette = &palette;
        // Computed once so both rail modes show the same active-download state.
        let download_badge = download_rail_badge(&state.downloads);
        let rail = egui::Panel::left("workspace_rail")
            .exact_size(width)
            .resizable(false)
            .frame(Frame::NONE.fill(palette.rail_bg))
            .show(root_ui, |ui| {
                if !state.rail_expanded {
                    // Favicon-only mode: show pinned tabs, then a new-tab
                    // button, then the remaining tabs. Command and DevTools
                    // live in the page's bottom-left HUD.
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
                                }) {
                                    ui.add_space(2.0);
                                    ui.separator();
                                    ui.add_space(2.0);
                                }
                                let new_tab = Self::rail_icon_button(
                                    ui,
                                    vec2(34.0, 30.0),
                                    RailIcon::Plus,
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
                                let incognito = Self::rail_icon_button(
                                    ui,
                                    vec2(34.0, 30.0),
                                    RailIcon::Private,
                                    palette,
                                );
                                if incognito.clicked() {
                                    actions.push(UiAction::NewPrivateWindow);
                                }
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &incognito,
                                    "New Incognito Window (Cmd/Ctrl+Shift+N)",
                                    state.rail_expanded,
                                );
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
                        ui.scope_builder(UiBuilder::new().max_rect(status_rect), |ui| {
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
                            });
                        });
                    });
                    return;
                }

                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    let rail_rect = ui.clip_rect();
                    ui.add_space(7.0);

                    // Framed omnibox: the helper reserves the fixed glyph slots
                    // and uses a zero-margin TextEdit so the frame stays inside
                    // the rail with even side margins.
                    Self::build_rail_address_field(ui, &ctx, state, actions, &palette);
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
                            }) {
                                ui.add_space(3.0);
                                ui.separator();
                                ui.add_space(3.0);
                            }
                            let new_tab = Self::rail_action_button(
                                ui,
                                width - 12.0,
                                30.0,
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
                            let incognito = Self::rail_action_button(
                                ui,
                                width - 12.0,
                                30.0,
                                RailIcon::Private,
                                "Incognito",
                                palette,
                            );
                            if incognito.clicked() {
                                actions.push(UiAction::NewPrivateWindow);
                            }
                            Self::chrome_tooltip(
                                tooltip_slot,
                                ui,
                                &incognito,
                                "New Incognito Window (Cmd/Ctrl+Shift+N)",
                                state.rail_expanded,
                            );
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
                ui.scope_builder(UiBuilder::new().max_rect(status_rect), |ui| {
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

    /// Draw a native WebRTC permission prompt above the page. Unlike a browser
    /// dialog, this lives in Kasumi's transparent native overlay so it can be
    /// keyboard-driven and keeps the page visible behind it.
    pub fn build_permission_prompt(
        ctx: &egui::Context,
        state: &ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        let Some(prompt) = state.permission_prompt.as_ref() else {
            return;
        };
        let request_id = prompt.request_id;
        let notification_prompt = prompt.kind == PermissionPromptKind::Notifications;
        let request_text = if notification_prompt {
            format!("Allow {} to show notifications?", prompt.origin)
        } else {
            format!("Allow {} to use {}?", prompt.origin, prompt.label)
        };
        let remember_text = if notification_prompt {
            if prompt.private {
                "This choice is forgotten when Incognito closes."
            } else {
                "Kasumi shows web notifications as native alerts and remembers your choice."
            }
        } else if prompt.private {
            "This choice is forgotten when Incognito closes."
        } else {
            "Kasumi remembers your choice for this site."
        };

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.dismiss_permission_prompt(actions);
            return;
        }

        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let width = PERMISSION_PROMPT_WIDTH.min((screen.width() - 24.0).max(240.0));
        let x = (screen.width() - width) / 2.0;
        let y = PERMISSION_PROMPT_TOP_OFFSET.min((screen.height() - 140.0).max(4.0));

        egui::Area::new(egui::Id::new("permission_scrim"))
            .order(Order::Foreground)
            .fixed_pos(screen.min)
            .movable(false)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::ZERO, palette.scrim);
            });

        let mut card_rect = egui::Rect::from_min_size(pos2(x, y), vec2(width, 0.0));
        let mut allow_clicked = false;
        let mut deny_clicked = false;
        egui::Area::new(egui::Id::new("permission_prompt"))
            .order(Order::Foreground)
            .fixed_pos(pos2(x, y))
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("🛡")
                                    .font(FontId::new(16.0, FontFamily::Proportional)),
                            );
                            ui.label(
                                RichText::new("Permission request")
                                    .size(13.0)
                                    .color(palette.text_strong),
                            );
                            if prompt.private {
                                ui.label(
                                    RichText::new("Incognito")
                                        .size(10.0)
                                        .color(palette.rail_accent),
                                );
                            }
                        });
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(&request_text)
                                .size(14.0)
                                .color(palette.text_strong),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(remember_text)
                                .size(10.0)
                                .color(palette.palette_subtext),
                        );
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    allow_clicked = material_button(
                                        ui,
                                        RailIcon::Check,
                                        "Allow",
                                        MaterialButtonStyle::Filled,
                                        &palette,
                                        true,
                                    )
                                    .clicked();
                                    deny_clicked = material_button(
                                        ui,
                                        RailIcon::Close,
                                        "Deny",
                                        MaterialButtonStyle::Tonal,
                                        &palette,
                                        true,
                                    )
                                    .clicked();
                                },
                            );
                        });
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        if deny_clicked {
            actions.push(if notification_prompt {
                UiAction::ResolveNotificationPermission {
                    request_id,
                    granted: false,
                    persist: true,
                }
            } else {
                UiAction::ResolveMediaAccessPermission {
                    request_id,
                    granted: false,
                    persist: true,
                }
            });
        } else if allow_clicked {
            actions.push(if notification_prompt {
                UiAction::ResolveNotificationPermission {
                    request_id,
                    granted: true,
                    persist: true,
                }
            } else {
                UiAction::ResolveMediaAccessPermission {
                    request_id,
                    granted: true,
                    persist: true,
                }
            });
        } else if ctx.input(|i| {
            command_palette_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        }) {
            state.dismiss_permission_prompt(actions);
        }
    }

    /// Draw Kasumi's native display/window picker for `getDisplayMedia`.
    pub fn build_display_media_picker(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        let Some(picker) = state.display_media_picker.clone() else {
            return;
        };
        let token = picker.token.clone();

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.dismiss_display_media_picker(actions);
            return;
        }
        if picker.sources.is_empty() {
            state.dismiss_display_media_picker(actions);
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            state.display_media_selection =
                (state.display_media_selection + 1).min(picker.sources.len() - 1);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            state.display_media_selection = state.display_media_selection.saturating_sub(1);
        }
        let selected = state.display_media_selection.min(picker.sources.len() - 1);
        state.display_media_selection = selected;

        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let width = DISPLAY_PICKER_WIDTH.min((screen.width() - 24.0).max(280.0));
        let x = (screen.width() - width) / 2.0;
        let y = DISPLAY_PICKER_TOP_OFFSET.min((screen.height() - 180.0).max(4.0));
        let max_height = DISPLAY_PICKER_MAX_HEIGHT.min((screen.height() - y - 16.0).max(200.0));

        egui::Area::new(egui::Id::new("display_picker_scrim"))
            .order(Order::Foreground)
            .fixed_pos(screen.min)
            .movable(false)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::ZERO, palette.scrim);
            });

        let mut card_rect = egui::Rect::from_min_size(pos2(x, y), vec2(width, 0.0));
        let mut share_clicked = false;
        let mut cancel_clicked = false;
        egui::Area::new(egui::Id::new("display_picker"))
            .order(Order::Foreground)
            .fixed_pos(pos2(x, y))
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Choose what to share")
                                .size(15.0)
                                .color(palette.text_strong),
                        );
                        ui.label(
                            RichText::new(
                                "Kasumi sends the selected display or window to the site.",
                            )
                            .size(10.0)
                            .color(palette.palette_subtext),
                        );
                        ui.add_space(10.0);
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .max_height(max_height - 132.0)
                            .show(ui, |ui| {
                                for (index, source) in picker.sources.iter().enumerate() {
                                    let icon = if source.is_window {
                                        RailIcon::DisplayWindow
                                    } else {
                                        RailIcon::DisplayScreen
                                    };
                                    let response = ui.selectable_label(
                                        index == selected,
                                        RichText::new(format!(
                                            "{}  {}",
                                            rail_icon_glyph(icon),
                                            source.label
                                        ))
                                        .size(11.0),
                                    );
                                    if response.clicked() {
                                        state.display_media_selection = index;
                                    }
                                    if response.double_clicked() {
                                        share_clicked = true;
                                        state.display_media_selection = index;
                                    }
                                }
                            });
                        ui.separator();
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    share_clicked |= material_button(
                                        ui,
                                        RailIcon::Check,
                                        "Share",
                                        MaterialButtonStyle::Filled,
                                        &palette,
                                        true,
                                    )
                                    .clicked();
                                    cancel_clicked = material_button(
                                        ui,
                                        RailIcon::Close,
                                        "Cancel",
                                        MaterialButtonStyle::Tonal,
                                        &palette,
                                        true,
                                    )
                                    .clicked();
                                },
                            );
                        });
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        if cancel_clicked {
            actions.push(UiAction::ResolveDisplayMediaPicker {
                token,
                device_id: None,
            });
        } else if share_clicked || ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            actions.push(UiAction::ResolveDisplayMediaPicker {
                token,
                device_id: Some(picker.sources[selected].device_id.clone()),
            });
        } else if ctx.input(|i| {
            command_palette_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        }) {
            state.dismiss_display_media_picker(actions);
        }
    }

    /// Draw Kasumi's native Linux print panel above the page. CEF has no
    /// platform print dialog on Linux, so this collects plain settings that
    /// `TabManager` applies to CEF and then spools through CUPS.
    pub fn build_print_dialog(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        let Some(dialog) = state.print_dialog.clone() else {
            return;
        };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.dismiss_print_dialog(actions);
            return;
        }

        let tab_id = dialog.tab_id;
        let has_selection = dialog.has_selection;
        let printers = dialog.printers.clone();
        let default_printer = dialog.default_printer.clone();
        let mut options = dialog.options.sanitized();

        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let width = PRINT_DIALOG_WIDTH.min((screen.width() - 24.0).max(280.0));
        let x = (screen.width() - width) / 2.0;
        let y = PERMISSION_PROMPT_TOP_OFFSET.min((screen.height() - 320.0).max(4.0));

        egui::Area::new(egui::Id::new("print_dialog_scrim"))
            .order(Order::Foreground)
            .fixed_pos(screen.min)
            .movable(false)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::ZERO, palette.scrim);
            });

        let mut card_rect = egui::Rect::from_min_size(pos2(x, y), vec2(width, 0.0));
        let mut print_clicked = false;
        egui::Area::new(egui::Id::new("print_dialog"))
            .order(Order::Foreground)
            .fixed_pos(pos2(x, y))
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(rail_icon_glyph(RailIcon::Print))
                                    .font(FontId::new(16.0, FontFamily::Monospace)),
                            );
                            ui.label(RichText::new("Print").size(14.0).color(palette.text_strong));
                        });
                        ui.label(
                            RichText::new("Render this page with CEF and send the PDF to CUPS.")
                                .size(10.0)
                                .color(palette.palette_subtext),
                        );
                        ui.add_space(10.0);
                        ui.separator();

                        let printer_label = if options.printer.is_empty() {
                            if default_printer.is_empty() {
                                "Default printer".to_string()
                            } else {
                                format!("Default ({default_printer})")
                            }
                        } else {
                            options.printer.clone()
                        };
                        egui::ComboBox::from_label("Printer")
                            .selected_text(printer_label)
                            .width(width - 120.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut options.printer,
                                    String::new(),
                                    "Default printer",
                                );
                                for printer in &printers {
                                    ui.selectable_value(
                                        &mut options.printer,
                                        printer.clone(),
                                        printer,
                                    );
                                }
                            });

                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let mut paper = options.paper;
                            let paper_label = match paper {
                                PrintPaperSize::Letter => "Letter",
                                PrintPaperSize::A4 => "A4",
                            };
                            egui::ComboBox::from_label("Paper")
                                .selected_text(paper_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut paper,
                                        PrintPaperSize::Letter,
                                        "Letter",
                                    );
                                    ui.selectable_value(&mut paper, PrintPaperSize::A4, "A4");
                                });
                            options.paper = paper;

                            ui.add_space(12.0);
                            ui.label("Copies");
                            ui.add(egui::DragValue::new(&mut options.copies).range(1..=999));
                        });
                        ui.add_space(8.0);
                        ui.checkbox(&mut options.landscape, "Landscape");
                        ui.checkbox(&mut options.color, "Print in color");
                        if has_selection {
                            ui.checkbox(&mut options.selection_only, "Print selection only");
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    print_clicked = material_button(
                                        ui,
                                        RailIcon::Print,
                                        "Print",
                                        MaterialButtonStyle::Filled,
                                        &palette,
                                        true,
                                    )
                                    .clicked();
                                    let cancel_clicked = material_button(
                                        ui,
                                        RailIcon::Close,
                                        "Cancel",
                                        MaterialButtonStyle::Tonal,
                                        &palette,
                                        true,
                                    )
                                    .clicked();
                                    if cancel_clicked {
                                        state.dismiss_print_dialog(actions);
                                    }
                                },
                            );
                        });
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        if let Some(active) = state.print_dialog.as_mut() {
            active.options = options.clone();
        }
        if print_clicked || ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            actions.push(UiAction::RespondPrintDialog {
                tab_id,
                accept: true,
                options: options.sanitized(),
            });
        } else if ctx.input(|i| {
            command_palette_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        }) {
            state.dismiss_print_dialog(actions);
        }
    }

    /// Draw the floating command palette over a dimmed full-window layer.
    pub fn build_command_palette(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        focus_command: bool,
    ) {
        let mut items = command_palette_items(
            &state.command_palette_query,
            &state.tabs,
            state.active_tab_id,
            &state.preferences,
        );
        if active_bookmark_key(state).is_some() {
            for item in &mut items {
                if item.action == CommandAction::BookmarkActiveTab {
                    item.icon = egui_material_icons::icons::ICON_BOOKMARK_REMOVE.into();
                    item.label = "Remove bookmark".to_string();
                    item.subtitle = "Remove the active page from its workspace folder".to_string();
                }
            }
        }
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

        let screen = ctx.viewport_rect();
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
                    .rect_filled(overlay_rect, CornerRadius::ZERO, palette.scrim);
            });

        let mut palette_rect =
            egui::Rect::from_min_size(pos2(palette_x, palette_y), vec2(palette_width, 0.0));
        egui::Area::new(egui::Id::new("command_palette"))
            .order(Order::Foreground)
            .fixed_pos(pos2(palette_x, palette_y))
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(palette_width);
                palette_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(14, 12))
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
                                .frame(Frame::NONE)
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
                                    egui::Sense::click(),
                                );
                                let fill = palette_row_fill(palette, selected, row_resp.hovered());
                                ui.painter()
                                    .rect_filled(row_rect, CornerRadius::same(6), fill);

                                // Lay the labels out *inside* the allocated row.
                                // Adding them after `allocate_exact_size` would
                                // place them below the highlight rect.
                                ui.scope_builder(UiBuilder::new().max_rect(row_rect), |ui| {
                                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(item.icon)
                                                .font(FontId::new(16.0, FontFamily::Proportional))
                                                .color(palette.text_strong),
                                        );
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

    /// Draw the floating find bar in the transparent overlay. CEF owns the fast
    /// substring highlight; the renderer shim supplies snippets, regex/word/
    /// multi-term highlights, and match navigation metadata.
    fn build_find_bar(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        focus_input: bool,
        favicon_textures: &HashMap<String, egui::TextureHandle>,
    ) {
        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let (width, x, y) = find_bar_geometry(screen);
        let anchor = pos2(x, y);

        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let shift = ctx.input(|i| i.modifiers.shift);

        let active_tab = state
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == state.active_tab_id);
        let snippets: Vec<FindSnippet> = filtered_find_snippets(
            active_tab,
            &state.find_filter_query,
            state.find_filter_enabled,
        )
        .into_iter()
        .cloned()
        .collect();
        let active_tab_id = state.active_tab_id.unwrap_or_default();

        let mut card_rect = egui::Rect::from_min_size(anchor, vec2(width, 0.0));
        let input_id = egui::Id::new("kasumi_find_input");

        egui::Area::new(egui::Id::new("find_bar"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if focus_input {
                                ctx.memory_mut(|mem| mem.request_focus(input_id));
                            }
                            let input = ui.add(
                                TextEdit::singleline(&mut state.find_query)
                                    .id(input_id)
                                    .hint_text("Find in page…")
                                    .text_color(palette.text_strong)
                                    .frame(Frame::NONE)
                                    .desired_width(width - 156.0),
                            );
                            if focus_input {
                                input.request_focus();
                            }
                            if input.changed() {
                                actions.push(UiAction::FindInPage {
                                    tab_id: active_tab_id,
                                    query: state.find_query.clone(),
                                    forward: true,
                                    match_case: state.find_match_case,
                                    find_next: false,
                                    options: state.find_options(),
                                });
                            }

                            let count_label = if state.find_pending {
                                "…".to_string()
                            } else if state.find_count < 0 {
                                "—".to_string()
                            } else if state.find_count == 0 {
                                "0/0".to_string()
                            } else {
                                let ordinal = state.find_active_ordinal.max(0);
                                format!("{ordinal}/{}", state.find_count.max(0))
                            };
                            ui.label(
                                RichText::new(count_label)
                                    .size(11.0)
                                    .color(palette.palette_subtext),
                            );

                            let prev = compact_find_icon_button(
                                ui,
                                RailIcon::ChevronLeft,
                                "Previous match (Shift+Enter)",
                                &palette,
                                false,
                            );
                            let next = compact_find_icon_button(
                                ui,
                                RailIcon::ChevronRight,
                                "Next match (Enter)",
                                &palette,
                                false,
                            );
                            let close = compact_find_icon_button(
                                ui,
                                RailIcon::Close,
                                "Close find bar (Esc)",
                                &palette,
                                false,
                            );
                            if prev.clicked() || (enter && shift) {
                                actions.push(UiAction::FindInPage {
                                    tab_id: active_tab_id,
                                    query: state.find_query.clone(),
                                    forward: false,
                                    match_case: state.find_match_case,
                                    find_next: true,
                                    options: state.find_options(),
                                });
                            }
                            if next.clicked() || (enter && !shift) {
                                actions.push(UiAction::FindInPage {
                                    tab_id: active_tab_id,
                                    query: state.find_query.clone(),
                                    forward: true,
                                    match_case: state.find_match_case,
                                    find_next: true,
                                    options: state.find_options(),
                                });
                            }
                            if close.clicked() {
                                actions.push(UiAction::StopFindingAll {
                                    clear_selection: true,
                                });
                                state.close_find_bar();
                            }
                        });

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let mut toggle = |label: &str, value: &mut bool, tooltip: &str| {
                                let text = if *value {
                                    RichText::new(label).size(10.0).color(palette.text_strong)
                                } else {
                                    RichText::new(label).size(10.0).color(palette.text_dim)
                                };
                                if flat_find_tooltip(ui.button(text), tooltip, &palette).clicked() {
                                    *value = !*value;
                                    true
                                } else {
                                    false
                                }
                            };
                            let case_changed =
                                toggle("Aa", &mut state.find_match_case, "Match case");
                            let regex_changed =
                                toggle(".*", &mut state.find_regex, "Regular expression");
                            let word_changed =
                                toggle("\\b", &mut state.find_whole_word, "Whole word only");
                            let multi_changed = toggle(
                                "A|B",
                                &mut state.find_multi_term,
                                "Multiple terms, one colour each",
                            );
                            let scope_changed = toggle(
                                "All tabs",
                                &mut state.find_all_tabs,
                                "Search every live tab",
                            );
                            let drawer_changed = toggle(
                                "Snippets",
                                &mut state.find_drawer_open,
                                "Show match context",
                            );
                            if case_changed
                                || regex_changed
                                || word_changed
                                || multi_changed
                                || scope_changed
                                || drawer_changed
                            {
                                actions.push(UiAction::FindInPage {
                                    tab_id: active_tab_id,
                                    query: state.find_query.clone(),
                                    forward: true,
                                    match_case: state.find_match_case,
                                    find_next: false,
                                    options: state.find_options(),
                                });
                            }
                        });

                        if !state.find_error.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(&state.find_error)
                                    .size(10.0)
                                    .color(Color32::from_rgb(255, 138, 128)),
                            );
                        }

                        if state.find_all_tabs {
                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Tabs with matches")
                                    .size(10.0)
                                    .color(palette.palette_subtext),
                            );
                            let mut last_workspace = String::new();
                            for tab in state
                                .tabs
                                .iter()
                                .filter(|tab| tab.find_count > 0 || tab.find_pending)
                                .take(12)
                            {
                                if last_workspace != tab.workspace_name {
                                    last_workspace.clone_from(&tab.workspace_name);
                                    ui.label(
                                        RichText::new(&last_workspace)
                                            .size(9.0)
                                            .color(palette.section_label),
                                    );
                                }
                                let response = ui.horizontal(|ui| {
                                    paint_find_favicon(ui, tab, favicon_textures, &palette);
                                    let title = if tab.title.trim().is_empty() {
                                        tab.url.as_str()
                                    } else {
                                        tab.title.as_str()
                                    };
                                    let label = if tab.find_pending {
                                        format!("{} — waking to search…", truncate_title(title, 38))
                                    } else {
                                        format!(
                                            "{} — {} match{}",
                                            truncate_title(title, 38),
                                            tab.find_count,
                                            if tab.find_count == 1 { "" } else { "es" }
                                        )
                                    };
                                    flat_find_tooltip(
                                        ui.button(RichText::new(label).size(11.0)),
                                        &tab.url,
                                        &palette,
                                    )
                                });
                                if response.inner.clicked() {
                                    actions.push(UiAction::SelectTab { tab_id: tab.id });
                                    let mut options = state.find_options();
                                    options.all_tabs = false;
                                    actions.push(UiAction::FindInPage {
                                        tab_id: tab.id,
                                        query: state.find_query.clone(),
                                        forward: true,
                                        match_case: state.find_match_case,
                                        find_next: false,
                                        options,
                                    });
                                }
                            }
                        }

                        if state.find_drawer_open {
                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if flat_find_tooltip(
                                    ui.button(RichText::new("Copy lines").size(10.0)),
                                    "Copy every matching line",
                                    &palette,
                                )
                                .clicked()
                                {
                                    let text = snippets
                                        .iter()
                                        .map(|snippet| {
                                            if snippet.line.is_empty() {
                                                format!(
                                                    "{}{}{}",
                                                    snippet.before, snippet.matched, snippet.after
                                                )
                                            } else {
                                                snippet.line.clone()
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    ctx.copy_text(text);
                                    state.show_toast(
                                        ToastKind::Success,
                                        "Copied",
                                        "Matching lines copied to clipboard",
                                    );
                                }
                                if flat_find_tooltip(
                                    ui.button(RichText::new("Copy sentences").size(10.0)),
                                    "Copy matching sentences",
                                    &palette,
                                )
                                .clicked()
                                {
                                    let text = snippets
                                        .iter()
                                        .map(|snippet| snippet.sentence.clone())
                                        .filter(|sentence| !sentence.is_empty())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    ctx.copy_text(text);
                                    state.show_toast(
                                        ToastKind::Success,
                                        "Copied",
                                        "Matching sentences copied to clipboard",
                                    );
                                }
                                if flat_find_tooltip(
                                    ui.button(RichText::new("Copy links").size(10.0)),
                                    "Copy URLs of matching links",
                                    &palette,
                                )
                                .clicked()
                                {
                                    let text = snippets
                                        .iter()
                                        .map(|snippet| snippet.href.clone())
                                        .filter(|href| !href.is_empty())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    ctx.copy_text(text);
                                    state.show_toast(
                                        ToastKind::Success,
                                        "Copied",
                                        "Matching links copied to clipboard",
                                    );
                                }
                                ui.toggle_value(
                                    &mut state.find_filter_enabled,
                                    RichText::new("Filter").size(10.0),
                                );
                            });
                            if state.find_filter_enabled {
                                ui.add_space(3.0);
                                ui.add(
                                    TextEdit::singleline(&mut state.find_filter_query)
                                        .hint_text("Filter snippets by text, heading, or link…")
                                        .font(FontId::proportional(11.0))
                                        .desired_width(width - 24.0),
                                );
                            }

                            egui::ScrollArea::vertical()
                                .id_salt("find_snippet_drawer")
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    if snippets.is_empty() {
                                        ui.label(
                                            RichText::new("No matching context yet")
                                                .size(10.0)
                                                .color(palette.palette_hint),
                                        );
                                        return;
                                    }
                                    for snippet in &snippets {
                                        if !snippet.heading.is_empty() {
                                            ui.label(
                                                RichText::new(&snippet.heading)
                                                    .size(9.0)
                                                    .color(palette.section_label),
                                            );
                                        }
                                        let response = ui
                                            .horizontal_wrapped(|ui| {
                                                ui.label(
                                                    RichText::new(&snippet.before)
                                                        .size(11.0)
                                                        .color(palette.text_dim),
                                                );
                                                ui.label(
                                                    RichText::new(&snippet.matched)
                                                        .size(11.0)
                                                        .color(find_term_color(snippet.term_index))
                                                        .strong(),
                                                );
                                                ui.label(
                                                    RichText::new(&snippet.after)
                                                        .size(11.0)
                                                        .color(palette.text_dim),
                                                );
                                            })
                                            .response;
                                        let hover = if snippet.href.is_empty() {
                                            snippet.line.clone()
                                        } else {
                                            format!("{}\n{}", snippet.line, snippet.href)
                                        };
                                        if flat_find_tooltip(response, hover, &palette).clicked() {
                                            actions.push(UiAction::JumpToFindMatch {
                                                tab_id: active_tab_id,
                                                ordinal: snippet.ordinal,
                                            });
                                        }
                                    }
                                });
                        }
                    })
                    .response
                    .rect
                    .expand(8.0);
            });

        if escape {
            actions.push(UiAction::StopFindingAll {
                clear_selection: true,
            });
            state.close_find_bar();
            return;
        }

        // A click outside the bar dismisses it, keeping the same overlay
        // behaviour as the command palette. The page keeps running behind it.
        let outside_click = ctx.input(|i| {
            command_palette_click_outside(
                card_rect,
                i.pointer.latest_pos(),
                i.pointer.primary_clicked(),
            )
        });
        if outside_click {
            actions.push(UiAction::StopFindingAll {
                clear_selection: true,
            });
            state.close_find_bar();
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

        let screen = ctx.viewport_rect();
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
                card_rect = Frame::NONE
                    .fill(palette.menu_bg)
                    .stroke(Stroke::new(1.0_f32, palette.menu_border))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(6, 6))
                    .show(ui, |ui| {
                        for (index, item) in items.iter().enumerate() {
                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                vec2(width - 16.0, RAIL_MENU_ROW_HEIGHT),
                                egui::Sense::click(),
                            );
                            let selected = index == state.menu_selection;
                            let fill = palette_row_fill(
                                palette,
                                selected,
                                row_resp.hovered() && item.enabled,
                            );
                            ui.painter()
                                .rect_filled(row_rect, CornerRadius::same(5), fill);

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
        let screen = ctx.viewport_rect();
        let size = downloads_bubble_size(screen);
        let anchor = downloads_bubble_anchor(screen, rail_width(state.rail_expanded));
        let mut card_rect = egui::Rect::from_min_size(anchor, size);

        egui::Area::new(egui::Id::new("kasumi_downloads_bubble"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(size.x);
                card_rect = Frame::NONE
                    .fill(palette.menu_bg)
                    .stroke(Stroke::new(1.0_f32, palette.menu_border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(10, 8))
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

    /// Paint a transient notification in the click-through overlay. The card is
    /// anchored to the top-right of the browser content so it never covers the
    /// native rail and remains visible above the CEF child view.
    pub fn build_toast(ctx: &egui::Context, state: &ChromeState) {
        let Some(toast) = state.toast.as_ref() else {
            return;
        };
        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let width = 330.0_f32.min((screen.width() - 32.0).max(220.0));
        let accent = match toast.kind {
            ToastKind::Success => palette.shield_ok,
            ToastKind::Error => palette.close_icon,
        };
        let icon = match toast.kind {
            ToastKind::Success => RailIcon::Check,
            ToastKind::Error => RailIcon::Close,
        };
        ctx.request_repaint_after(toast.duration.saturating_sub(toast.created_at.elapsed()));

        egui::Area::new(egui::Id::new("kasumi_native_toast"))
            .order(Order::Foreground)
            .anchor(Align2::RIGHT_TOP, vec2(-16.0, 16.0))
            .fixed_pos(pos2(screen.right() - 16.0, screen.top() + 16.0))
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, accent))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (icon_rect, _) =
                                ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                            paint_rail_icon(
                                ui,
                                egui::Rect::from_center_size(icon_rect.center(), vec2(14.0, 14.0)),
                                icon,
                                accent,
                            );
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(&toast.title)
                                        .size(12.0)
                                        .strong()
                                        .color(palette.text_strong),
                                );
                                ui.label(
                                    RichText::new(&toast.message)
                                        .size(10.0)
                                        .color(palette.palette_subtext),
                                );
                            });
                        });
                    });
            });
    }

    /// Render the bookmark manager list and its single-row editor. Both the
    /// dedicated dialog and the former Settings page used this shared body, which
    /// keeps search, edit, move, and delete behavior in one place.
    fn build_bookmark_manager_contents(
        ui: &mut Ui,
        bookmarks: &[BookmarkInfo],
        workspaces: &[WorkspaceInfo],
        active_tab_id: Option<u64>,
        bookmark_search: &mut String,
        selected_bookmark_key: &mut Option<(u64, String)>,
        bookmark_title: &mut String,
        bookmark_url: &mut String,
        bookmark_workspace_id: &mut u64,
        bookmark_folder: &mut String,
        actions: &mut Vec<UiAction>,
        palette: &ChromePalette,
    ) {
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
                actions.push(UiAction::AddBookmark { tab_id });
            }
        }
        ui.label(
            RichText::new(
                "Bookmarks stay visible from every workspace; folders are named after workspaces.",
            )
            .size(10.0)
            .color(palette.palette_hint),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Search")
                    .size(10.0)
                    .color(palette.section_label),
            );
            ui.add(
                TextEdit::singleline(bookmark_search)
                    .id(egui::Id::new("kasumi_bookmark_search"))
                    .hint_text("Title, URL, or folder")
                    .desired_width((ui.available_width() - 12.0).max(80.0)),
            );
        });
        ui.add_space(6.0);

        let filtered = filter_bookmarks(&bookmarks, bookmark_search);
        if filtered.is_empty() {
            ui.label(
                RichText::new(if bookmarks.is_empty() {
                    "No bookmarks yet. Bookmark the current tab to get started."
                } else {
                    "No bookmarks match that search."
                })
                .size(10.0)
                .color(palette.palette_hint),
            );
        }

        let mut folders: BTreeMap<String, Vec<BookmarkInfo>> = BTreeMap::new();
        for bookmark in filtered {
            folders
                .entry(bookmark.folder.clone())
                .or_default()
                .push(bookmark);
        }
        for (folder, items) in folders {
            let heading = format!("{}  ·  {}", folder, items.len());
            egui::CollapsingHeader::new(heading)
                .id_salt(("bookmark_folder", folder.clone()))
                .default_open(true)
                .show(ui, |ui| {
                    for bookmark in items {
                        let key = (bookmark.workspace_id, bookmark.url.clone());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(truncate_title(&bookmark.title, 22)).size(11.0));
                            ui.label(
                                RichText::new(truncate_title(&bookmark.url, 30))
                                    .size(9.0)
                                    .color(palette.palette_hint),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if compact_icon_button(
                                    ui,
                                    RailIcon::OpenInNew,
                                    "Open bookmark",
                                    palette,
                                    true,
                                )
                                .clicked()
                                {
                                    actions.push(UiAction::OpenBookmark {
                                        url: bookmark.url.clone(),
                                    });
                                }
                                if compact_icon_button(
                                    ui,
                                    RailIcon::Delete,
                                    "Delete bookmark",
                                    palette,
                                    false,
                                )
                                .clicked()
                                {
                                    actions.push(UiAction::DeleteBookmark {
                                        workspace_id: bookmark.workspace_id,
                                        url: bookmark.url.clone(),
                                    });
                                    if selected_bookmark_key.as_ref() == Some(&key) {
                                        *selected_bookmark_key = None;
                                    }
                                }
                                if compact_icon_button(
                                    ui,
                                    RailIcon::Edit,
                                    "Edit bookmark",
                                    palette,
                                    false,
                                )
                                .clicked()
                                {
                                    *selected_bookmark_key = Some(key);
                                    *bookmark_title = bookmark.title.clone();
                                    *bookmark_url = bookmark.url.clone();
                                    *bookmark_workspace_id = bookmark.workspace_id;
                                    *bookmark_folder = bookmark.folder.clone();
                                }
                            });
                        });
                    }
                });
        }

        if let Some((workspace_id, url)) = &selected_bookmark_key {
            let original = bookmarks
                .iter()
                .find(|bookmark| bookmark.workspace_id == *workspace_id && bookmark.url == *url);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(
                RichText::new("EDIT BOOKMARK")
                    .size(10.0)
                    .strong()
                    .color(palette.section_label),
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Title").size(10.0));
            ui.add(
                TextEdit::singleline(bookmark_title)
                    .id(egui::Id::new("kasumi_bookmark_title"))
                    .desired_width(ui.available_width()),
            );
            ui.add_space(4.0);
            ui.label(RichText::new("URL").size(10.0));
            ui.add(
                TextEdit::singleline(bookmark_url)
                    .id(egui::Id::new("kasumi_bookmark_url"))
                    .desired_width(ui.available_width()),
            );

            let mut folder_options: Vec<(u64, String)> = workspaces
                .iter()
                .map(|workspace| (workspace.id, workspace.name.clone()))
                .collect();
            if let Some(original) = original {
                let archived = (original.workspace_id, original.folder.clone());
                if !folder_options.iter().any(|option| *option == archived) {
                    folder_options.insert(0, archived);
                }
            }
            if folder_options.is_empty() {
                folder_options.push((*bookmark_workspace_id, bookmark_folder.clone()));
            }
            if !folder_options
                .iter()
                .any(|(id, folder)| *id == *bookmark_workspace_id && *folder == *bookmark_folder)
            {
                if let Some(first) = folder_options.first() {
                    *bookmark_workspace_id = first.0;
                    *bookmark_folder = first.1.clone();
                }
            }
            let mut selected_folder = bookmark_folder.clone();
            egui::ComboBox::from_label("Folder")
                .selected_text(selected_folder.clone())
                .show_ui(ui, |ui| {
                    for (id, folder) in &folder_options {
                        if ui
                            .selectable_value(&mut selected_folder, folder.clone(), folder)
                            .clicked()
                        {
                            *bookmark_workspace_id = *id;
                            *bookmark_folder = folder.clone();
                        }
                    }
                });

            let normalized_url = normalize_bookmark_url(bookmark_url);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if material_button(
                    ui,
                    RailIcon::Check,
                    "Save bookmark",
                    MaterialButtonStyle::Tonal,
                    palette,
                    normalized_url.is_some(),
                )
                .clicked()
                {
                    if let (Some((original_workspace_id, original_url)), Some(url)) =
                        (selected_bookmark_key.clone(), normalized_url)
                    {
                        actions.push(UiAction::SaveBookmark {
                            original_workspace_id,
                            original_url,
                            workspace_id: *bookmark_workspace_id,
                            url,
                            title: bookmark_title.trim().to_string(),
                            folder: bookmark_folder.clone(),
                        });
                        *selected_bookmark_key = None;
                        bookmark_title.clear();
                        bookmark_url.clear();
                    }
                }
                if material_button(
                    ui,
                    RailIcon::Close,
                    "Cancel",
                    MaterialButtonStyle::Outlined,
                    palette,
                    true,
                )
                .clicked()
                {
                    *selected_bookmark_key = None;
                    bookmark_title.clear();
                    bookmark_url.clear();
                }
            });
            ui.add_space(4.0);
            if material_button(
                ui,
                RailIcon::Delete,
                "Delete bookmark",
                MaterialButtonStyle::Outlined,
                palette,
                true,
            )
            .clicked()
            {
                if let Some((workspace_id, url)) = selected_bookmark_key.clone() {
                    actions.push(UiAction::DeleteBookmark { workspace_id, url });
                }
                *selected_bookmark_key = None;
                bookmark_title.clear();
                bookmark_url.clear();
            }
        }
    }

    /// Draw the browser-import page. Detection, parsing, and SQLite writes are
    /// all performed off-thread; this function only edits native UI state and
    /// emits `UiAction`s.
    fn build_browser_import_page(
        ui: &mut Ui,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        palette: &ChromePalette,
    ) {
        ui.label(
            RichText::new("BROWSER DATA")
                .size(10.0)
                .strong()
                .color(palette.section_label),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new("Bring bookmarks and history from a locally installed browser. Passwords are not imported.")
                .size(11.0)
                .color(palette.palette_subtext),
        );
        ui.add_space(8.0);

        let detect_label = if state.browser_import_profiles.is_empty() {
            "Detect installed browsers"
        } else {
            "Refresh browser detection"
        };
        if material_button(
            ui,
            RailIcon::Download,
            detect_label,
            MaterialButtonStyle::Tonal,
            palette,
            !state.browser_import_detecting && !state.browser_import_running,
        )
        .clicked()
        {
            state.browser_import_detecting = true;
            state.browser_import_progress = "Discovering local browser profiles…".to_string();
            state.browser_import_report = None;
            actions.push(UiAction::DetectBrowserProfiles);
        }

        ui.add_space(8.0);
        ui.label(
            RichText::new("DETECTED PROFILES")
                .size(10.0)
                .strong()
                .color(palette.section_label),
        );
        ui.add_space(3.0);

        let profiles = state.browser_import_profiles.clone();
        if state.browser_import_detecting {
            ui.label(
                RichText::new(&state.browser_import_progress)
                    .size(11.0)
                    .color(palette.palette_subtext),
            );
        } else if profiles.is_empty() {
            ui.label(
                RichText::new(
                    "No supported profiles found. Install or launch Chrome, Edge, Firefox, or Safari, then refresh.",
                )
                .size(11.0)
                .color(palette.palette_hint),
            );
        }
        for profile in &profiles {
            let path = profile
                .bookmarks_path
                .as_deref()
                .or(profile.history_path.as_deref())
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Local profile".to_string());
            let selected = state.browser_import_selected_source == Some(profile.source);
            if ui
                .selectable_label(selected, format!("{} — {}", profile.source.label(), path))
                .clicked()
                && !state.browser_import_running
            {
                state.browser_import_selected_source = Some(profile.source);
                state.browser_import_bookmarks = profile.supports_bookmarks();
                state.browser_import_history = profile.supports_history();
                state.reset_browser_import_result();
            }
        }

        let selected_profile = state.browser_import_selected_source.and_then(|source| {
            profiles
                .iter()
                .find(|profile| profile.source == source)
                .cloned()
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new("IMPORT")
                .size(10.0)
                .strong()
                .color(palette.section_label),
        );
        ui.add_space(3.0);

        let mut want_bookmarks = state.browser_import_bookmarks;
        let mut want_history = state.browser_import_history;
        let inputs_enabled = !state.browser_import_running && !state.browser_import_detecting;
        let bookmarks_available = inputs_enabled
            && selected_profile
                .as_ref()
                .is_some_and(BrowserProfile::supports_bookmarks);
        let history_available = inputs_enabled
            && selected_profile
                .as_ref()
                .is_some_and(BrowserProfile::supports_history);
        ui.add_enabled(
            bookmarks_available,
            egui::Checkbox::new(&mut want_bookmarks, "Bookmarks"),
        );
        ui.add_enabled(
            history_available,
            egui::Checkbox::new(&mut want_history, "History"),
        );
        state.browser_import_bookmarks = want_bookmarks;
        state.browser_import_history = want_history;

        let can_import =
            selected_profile.is_some() && (want_bookmarks || want_history) && inputs_enabled;
        ui.horizontal(|ui| {
            if material_button(
                ui,
                RailIcon::Download,
                "Import selected data",
                MaterialButtonStyle::Filled,
                palette,
                can_import,
            )
            .clicked()
            {
                if let Some(profile) = selected_profile.clone() {
                    state.browser_import_running = true;
                    state.browser_import_progress = "Preparing import…".to_string();
                    state.browser_import_report = None;
                    actions.push(UiAction::StartBrowserImport {
                        profile,
                        bookmarks: want_bookmarks,
                        history: want_history,
                    });
                }
            }
            if state.browser_import_running
                && material_button(
                    ui,
                    RailIcon::Close,
                    "Cancel",
                    MaterialButtonStyle::Tonal,
                    palette,
                    true,
                )
                .clicked()
            {
                state.browser_import_progress = "Canceling import…".to_string();
                actions.push(UiAction::CancelBrowserImport);
            }
        });

        if !state.browser_import_progress.is_empty() && !state.browser_import_detecting {
            ui.add_space(8.0);
            ui.label(
                RichText::new(&state.browser_import_progress)
                    .size(11.0)
                    .color(palette.palette_subtext),
            );
        }
        if let Some(report) = &state.browser_import_report {
            ui.add_space(8.0);
            if report.canceled {
                ui.label(
                    RichText::new("Import canceled. No partial data was applied.")
                        .size(11.0)
                        .color(palette.palette_hint),
                );
            } else if report.success() {
                ui.label(
                    RichText::new(format!(
                        "Imported {} bookmark(s) and {} history entries.",
                        report.bookmarks_added, report.history_added
                    ))
                    .size(11.0)
                    .color(palette.shield_ok),
                );
            } else {
                for error in &report.errors {
                    ui.label(
                        RichText::new(error)
                            .size(11.0)
                            .color(Color32::from_rgb(255, 148, 148)),
                    );
                }
            }
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new("Imported bookmarks are placed in an “Imported Browser” folder in the current workspace. Duplicate URLs are updated in place.")
                .size(10.0)
                .color(palette.palette_hint),
        );

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(
            RichText::new("EXPORT")
                .size(10.0)
                .strong()
                .color(palette.section_label),
        );
        ui.add_space(3.0);
        ui.label(
            RichText::new(
                "Save portable copies of your bookmarks and history in the download folder.",
            )
            .size(11.0)
            .color(palette.palette_subtext),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if material_button(
                ui,
                RailIcon::Upload,
                "Bookmarks (HTML)",
                MaterialButtonStyle::Tonal,
                palette,
                true,
            )
            .clicked()
            {
                actions.push(UiAction::ExportBrowserData {
                    bookmarks: true,
                    history: false,
                });
            }
            if material_button(
                ui,
                RailIcon::Upload,
                "History (CSV)",
                MaterialButtonStyle::Tonal,
                palette,
                true,
            )
            .clicked()
            {
                actions.push(UiAction::ExportBrowserData {
                    bookmarks: false,
                    history: true,
                });
            }
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new("Bookmarks use Netscape HTML, which Chrome, Edge, Firefox, and Safari can import. Browsers do not share a history import format, so history is exported as CSV with Unix and UTC timestamps.")
                .size(10.0)
                .color(palette.palette_hint),
        );
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
        // Incognito is intentionally ephemeral: it appears in the rail but not
        // in Settings, where workspaces can be renamed or persisted.
        let workspaces: Vec<WorkspaceInfo> = state
            .workspaces
            .iter()
            .filter(|workspace| !Workspace::is_private_id(workspace.id))
            .cloned()
            .collect();
        let active_workspace = workspaces
            .iter()
            .find(|workspace| workspace.id == state.active_workspace_id)
            .cloned();
        let workspace_count = workspaces.len();
        let mut settings_tab = state.settings_tab;
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
        let screen = ctx.viewport_rect();
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
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(20, 18))
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
                                (SettingsTab::Import, "Import / Export"),
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
                            SettingsTab::Import => "kasumi_settings_import",
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
                                                ui.allocate_exact_size(vec2(width, 32.0), egui::Sense::click());
                                            let fill = if is_selected {
                                                palette.rail_row_active
                                            } else if response.hovered() {
                                                palette.rail_row_hover
                                            } else {
                                                Color32::TRANSPARENT
                                            };
                                            ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
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
                                SettingsTab::Import => {
                                    ChromeRenderer::build_browser_import_page(
                                        ui, state, actions, palette,
                                    );
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

    /// Draw the dedicated bookmark manager dialog in the shared native overlay.
    ///
    /// This is intentionally separate from Settings: Cmd/Ctrl+Shift+B and the
    /// command palette open a focused manager card without exposing preferences.
    pub fn build_bookmark_manager(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        if !state.bookmark_manager_open {
            return;
        }

        let bookmarks = state.bookmarks.clone();
        let workspaces = state.workspaces.clone();
        let active_tab_id = state.active_tab_id;
        let mut bookmark_search = state.bookmark_search_query.clone();
        let mut selected_bookmark_key = state.selected_bookmark_key.clone();
        let mut bookmark_title = state.bookmark_title_draft.clone();
        let mut bookmark_url = state.bookmark_url_draft.clone();
        let mut bookmark_workspace_id = state.bookmark_workspace_id_draft;
        let mut bookmark_folder = state.bookmark_folder_draft.clone();

        if let Some((workspace_id, url)) = &selected_bookmark_key {
            let still_present = bookmarks
                .iter()
                .any(|bookmark| bookmark.workspace_id == *workspace_id && bookmark.url == *url);
            if !still_present {
                selected_bookmark_key = None;
                bookmark_title.clear();
                bookmark_url.clear();
                bookmark_workspace_id = state.active_workspace_id;
                bookmark_folder.clear();
            }
        }

        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let width = SETTINGS_WIDTH.min((screen.width() - 24.0).max(280.0));
        let content_height = SETTINGS_MAX_CONTENT_HEIGHT.min((screen.height() - 120.0).max(160.0));
        let mut card_rect = egui::Rect::from_min_size(screen.center(), vec2(width, 0.0));
        let mut done_clicked = false;
        let requested_close = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        egui::Area::new(egui::Id::new("kasumi_bookmark_manager"))
            .order(Order::Foreground)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .fixed_pos(screen.center())
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(20, 18))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Bookmark manager")
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
                                    &palette,
                                    true,
                                )
                                .clicked();
                            });
                        });
                        ui.add_space(4.0);
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt("kasumi_bookmark_manager_contents")
                            .max_height(content_height)
                            .show(ui, |ui| {
                                Self::build_bookmark_manager_contents(
                                    ui,
                                    &bookmarks,
                                    &workspaces,
                                    active_tab_id,
                                    &mut bookmark_search,
                                    &mut selected_bookmark_key,
                                    &mut bookmark_title,
                                    &mut bookmark_url,
                                    &mut bookmark_workspace_id,
                                    &mut bookmark_folder,
                                    actions,
                                    &palette,
                                );
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

        state.bookmark_search_query = bookmark_search;
        state.selected_bookmark_key = selected_bookmark_key;
        state.bookmark_title_draft = bookmark_title;
        state.bookmark_url_draft = bookmark_url;
        state.bookmark_workspace_id_draft = bookmark_workspace_id;
        state.bookmark_folder_draft = bookmark_folder;

        if requested_close || done_clicked || outside_click {
            state.close_bookmark_manager();
        }
    }

    /// Draw the dedicated history manager dialog in the shared native overlay.
    /// Searching and row actions stay in memory; SQLite work is queued through
    /// `UiAction`s so neither winit nor CEF ever blocks on disk.
    pub fn build_history(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
    ) {
        if !state.history_open {
            return;
        }

        // Borrow the snapshot instead of cloning every row each frame. Egui
        // only needs the rows we actually lay out, so filtering produces
        // indices and pagination caps widget construction at a small page.
        let history = &state.history;
        let mut query = state.history_query.clone();
        let query_before = query.clone();
        let mut visible_count = state.history_visible_count.max(HISTORY_VISIBLE_PAGE_SIZE);
        let mut show_more_clicked = false;
        let mut clear_confirm = state.history_clear_confirm;
        let now = crate::storage::db::now_epoch_secs();
        let palette = state.palette();
        let screen = ctx.viewport_rect();
        let width = SETTINGS_WIDTH.min((screen.width() - 24.0).max(280.0));
        let content_height = SETTINGS_MAX_CONTENT_HEIGHT.min((screen.height() - 120.0).max(160.0));
        let mut card_rect = egui::Rect::from_min_size(screen.center(), vec2(width, 0.0));
        let mut done_clicked = false;
        let mut clear_clicked = false;
        let mut opened_url = None;
        let mut deleted_urls = Vec::new();
        let requested_close = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        egui::Area::new(egui::Id::new("kasumi_history_manager"))
            .order(Order::Foreground)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .fixed_pos(screen.center())
            .movable(false)
            .show(ctx, |ui| {
                ui.set_width(width);
                card_rect = Frame::NONE
                    .fill(palette.palette_bg)
                    .stroke(Stroke::new(1.0_f32, palette.palette_border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(20, 18))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("History")
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
                                    &palette,
                                    true,
                                )
                                .clicked();
                            });
                        });
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.add_sized(
                            [width - 40.0, 28.0],
                            egui::TextEdit::singleline(&mut query)
                                .hint_text("Search titles and URLs")
                                .font(FontId::new(12.0, FontFamily::Proportional)),
                        );
                        ui.add_space(8.0);

                        if query != query_before {
                            visible_count = HISTORY_VISIBLE_PAGE_SIZE;
                        }
                        let matches = filter_history_indices(history, &query);
                        let total_matches = matches.len();
                        let shown = visible_count.min(total_matches);

                        egui::ScrollArea::vertical()
                            .id_salt("kasumi_history_manager_contents")
                            .max_height(content_height)
                            .show(ui, |ui| {
                                if matches.is_empty() {
                                    ui.label(
                                        RichText::new(if history.is_empty() {
                                            "No visited pages loaded yet."
                                        } else {
                                            "No history entries match this search."
                                        })
                                        .color(palette.palette_subtext),
                                    );
                                    return;
                                }
                                for &index in matches.iter().take(visible_count) {
                                    let item = &history[index];
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            let title = if item.title.trim().is_empty() {
                                                item.url.clone()
                                            } else {
                                                item.title.clone()
                                            };
                                            ui.label(
                                                RichText::new(truncate_title(&title, 64))
                                                    .size(12.0)
                                                    .color(palette.text_strong),
                                            );
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if ui.button("Delete").clicked() {
                                                        deleted_urls.push(item.url.clone());
                                                    }
                                                    if ui.button("Open").clicked() {
                                                        opened_url = Some(item.url.clone());
                                                    }
                                                },
                                            );
                                        });
                                        ui.label(
                                            RichText::new(truncate_title(&item.url, 78))
                                                .size(10.0)
                                                .color(palette.palette_subtext),
                                        );
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} visit{} · {}",
                                                    item.visit_count,
                                                    if item.visit_count == 1 { "" } else { "s" },
                                                    format_history_time(item.last_visited, now)
                                                ))
                                                .size(10.0)
                                                .color(palette.palette_hint),
                                            );
                                        });
                                    });
                                    ui.separator();
                                }
                            });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("Showing {shown} of {total_matches}"))
                                    .size(10.0)
                                    .color(palette.palette_hint),
                            );
                            if total_matches > visible_count && ui.button("Show more").clicked() {
                                show_more_clicked = true;
                            }
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button(if clear_confirm {
                                    "Confirm clear all history"
                                } else {
                                    "Clear all history"
                                })
                                .clicked()
                            {
                                if clear_confirm {
                                    clear_clicked = true;
                                    clear_confirm = false;
                                } else {
                                    clear_confirm = true;
                                }
                            }
                            if clear_confirm {
                                ui.label(
                                    RichText::new("Click again to remove every visited page.")
                                        .size(10.0)
                                        .color(palette.palette_subtext),
                                );
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

        if show_more_clicked {
            visible_count = visible_count.saturating_add(HISTORY_VISIBLE_PAGE_SIZE);
        }
        state.history_query = query;
        state.history_visible_count = visible_count;
        state.history_clear_confirm = clear_confirm;

        if let Some(url) = opened_url {
            actions.push(UiAction::OpenHistoryUrl { url });
            state.close_history();
        }
        for url in deleted_urls {
            actions.push(UiAction::DeleteHistory { url });
        }
        if clear_clicked {
            actions.push(UiAction::ClearHistory);
        }
        if requested_close || done_clicked || outside_click {
            state.close_history();
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
        let screen = ui.ctx().viewport_rect();
        let text = text.into();
        let (width, height) = tooltip_size_for_text(ui.ctx(), &text, expanded);
        let anchor = page_tooltip_anchor(rail, response.rect, screen, width, height);
        *tooltip = Some(RailTooltip {
            text,
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
        Self::rail_icon_button_with_inset(ui, size, icon, palette, 5.0)
    }

    /// Same button with a configurable glyph inset, used by the compact
    /// bottom HUD where the default 5 DIP inset would shrink icons too far.
    fn rail_icon_button_with_inset(
        ui: &mut Ui,
        size: egui::Vec2,
        icon: RailIcon,
        palette: &ChromePalette,
        inset: f32,
    ) -> Response {
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        let fill = if response.hovered() {
            palette.rail_surface
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(
            rect,
            CornerRadius::same((size.min_elem() * 0.24).max(3.0) as u8),
            fill,
        );
        paint_rail_icon(
            ui,
            rect.shrink(inset),
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
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), egui::Sense::click());
        let fill = if response.hovered() {
            palette.rail_surface
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
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
            egui::Sense::click(),
        );
        if row_hovered || response.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), palette.close_bg);
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
        ui.painter().rect_filled(rect, CornerRadius::same(7), fill);

        if is_active {
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                CornerRadius::same(7),
                Stroke::new(1.0_f32, palette.tab_border),
                egui::StrokeKind::Inside,
            );
            ui.painter().rect_filled(
                egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
                CornerRadius::same(2),
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
        let (row_rect, row_resp) = ui.allocate_exact_size(
            vec2(width - horizontal_inset, row_height),
            egui::Sense::click(),
        );

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
                egui::Sense::click(),
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
            let (menu_width, menu_height) = rail_menu_metrics(state, tab.id);
            let anchor = rail_menu_anchor(
                rail_rect,
                pointer,
                ui.ctx().viewport_rect(),
                menu_width,
                menu_height,
            );
            *menu_request = Some((tab_id, anchor));
        }

        let private = Workspace::is_private_id(tab.workspace_id);
        let label = if private && tab.is_pinned {
            format!("Incognito · Pinned · {}", tab.title)
        } else if private {
            format!("Incognito · {}", tab.title)
        } else if tab.is_pinned {
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
    /// Find-bar result rows need the same cached site icons as the rail.
    favicon_textures: HashMap<String, egui::TextureHandle>,
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
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let egui_ctx = egui::Context::default();
        register_material_icon_font(&egui_ctx);

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
            favicon_textures: HashMap::new(),
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

    /// Keep the find-bar result list's favicons in sync with tab metadata.
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

    /// Render the palette or tab menu into the transparent overlay window.
    pub fn render(
        &mut self,
        window: &winit::window::Window,
        state: &mut ChromeState,
    ) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let mut raw_input = self.egui_state.take_egui_input(window);
        if std::mem::take(&mut state.find_dismiss_requested) {
            actions.push(UiAction::StopFindingAll {
                clear_selection: true,
            });
        }
        state.expire_toast();
        state.promote_pending_toast();
        // Only one surface may own the shared overlay. A native permission
        // prompt always wins because a WebRTC request must not be hidden behind
        // Settings or the palette. Linux's print panel and the display picker
        // also own CEF callbacks, so they take priority over ordinary chrome.
        let permission_open = state.permission_prompt.is_some();
        let print_open = state.print_dialog.is_some() && !permission_open;
        let display_picker_open =
            state.display_media_picker.is_some() && !permission_open && !print_open;
        let modal_open = permission_open || print_open || display_picker_open;
        if modal_open {
            state.dismiss_find_for_overlay();
        }
        let find_open = state.find_bar_open && !modal_open;
        let settings_open = state.settings_open && !modal_open && !find_open;
        let bookmark_manager_open =
            state.bookmark_manager_open && !settings_open && !modal_open && !find_open;
        let history_open = state.history_open
            && !settings_open
            && !bookmark_manager_open
            && !modal_open
            && !find_open;
        let menu_open = state.menu.is_some()
            && !settings_open
            && !bookmark_manager_open
            && !history_open
            && !modal_open
            && !find_open;
        let palette_open = state.command_palette_open
            && !menu_open
            && !settings_open
            && !bookmark_manager_open
            && !history_open
            && !modal_open
            && !find_open;
        let downloads_open = state.downloads_open
            && !palette_open
            && !menu_open
            && !settings_open
            && !bookmark_manager_open
            && !history_open
            && !modal_open;
        let toast_open = state.toast.is_some()
            && !settings_open
            && !bookmark_manager_open
            && !history_open
            && !menu_open
            && !palette_open
            && !find_open
            && !downloads_open
            && !modal_open;
        let focus_before = state.command_palette_focus_requested && palette_open;
        let select_all_before = state.command_palette_select_all_requested && palette_open;
        let focus_find = state.find_focus_requested && find_open;
        self.sync_favicon_textures(state);
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
        let mut full_output = egui_ctx.run_ui(raw_input, |ui| {
            let ctx = ui.ctx().clone();
            if permission_open {
                ChromeRenderer::build_permission_prompt(&ctx, state, &mut actions);
            } else if print_open {
                ChromeRenderer::build_print_dialog(&ctx, state, &mut actions);
            } else if display_picker_open {
                ChromeRenderer::build_display_media_picker(&ctx, state, &mut actions);
            } else if settings_open {
                ChromeRenderer::build_settings(&ctx, state, &mut actions);
            } else if bookmark_manager_open {
                ChromeRenderer::build_bookmark_manager(&ctx, state, &mut actions);
            } else if history_open {
                ChromeRenderer::build_history(&ctx, state, &mut actions);
            } else if menu_open {
                ChromeRenderer::build_tab_menu(&ctx, state, &mut actions);
            } else if palette_open {
                ChromeRenderer::build_command_palette(&ctx, state, &mut actions, focus_command);
            } else if find_open {
                ChromeRenderer::build_find_bar(
                    &ctx,
                    state,
                    &mut actions,
                    focus_find,
                    &self.favicon_textures,
                );
            } else if downloads_open {
                ChromeRenderer::build_downloads_bubble(&ctx, state, &mut actions);
            } else if toast_open {
                ChromeRenderer::build_toast(&ctx, state);
            }
        });
        // The rail button raises this flag during the main window's render,
        // which happens before this overlay's first frame. Consume it here.
        let select_all_next = focus_before && palette_open;
        state.command_palette_focus_requested =
            focus_request_after_render(focus_before, state.command_palette_focus_requested);
        state.command_palette_select_all_requested = select_all_next;
        state.find_focus_requested =
            focus_request_after_render(focus_find, state.find_focus_requested);

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        for (id, deltas) in full_output.textures_delta.set.drain() {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let Some(output_frame) = acquire_surface_frame(
            &self.surface,
            &self.device,
            &self.surface_config,
            "palette overlay",
        ) else {
            return actions;
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
                        depth_slice: None,
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
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in full_output.textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(output_frame);

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
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
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
        let mut full_output = egui_ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::no_frame()
                .frame(Frame::NONE.fill(Color32::TRANSPARENT))
                .show(ui, |ui| {
                    Frame::NONE
                        .fill(palette.tooltip_bg)
                        .stroke(Stroke::new(1.0_f32, palette.tooltip_border))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(
                            TOOLTIP_TEXT_MARGIN_X as i8,
                            TOOLTIP_TEXT_MARGIN_Y as i8,
                        ))
                        .show(ui, |ui| {
                            // Fill the whole overlay window so the visible card is
                            // centered on the hovered tab row. Reuse the same
                            // wrapped galley that sized the window, so long text
                            // cannot overflow the card.
                            let galley = with_tooltip_galley(ui.ctx(), &card.text, |galley| galley);
                            let inner_size = vec2(
                                (card.width - 2.0 * TOOLTIP_TEXT_MARGIN_X).max(40.0),
                                (card.height - 2.0 * TOOLTIP_TEXT_MARGIN_Y).max(10.0),
                            );
                            let (rect, _) = ui.allocate_exact_size(inner_size, Sense::hover());
                            let text_pos = pos2(
                                rect.left() + (rect.width() - galley.rect.width()) * 0.5,
                                rect.top() + (rect.height() - galley.rect.height()) * 0.5,
                            );
                            ui.painter().galley_with_override_text_color(
                                text_pos,
                                galley,
                                palette.tooltip_text,
                            );
                        });
                });
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        for (id, deltas) in full_output.textures_delta.set.drain() {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let Some(output_frame) = acquire_surface_frame(
            &self.surface,
            &self.device,
            &self.surface_config,
            "tooltip overlay",
        ) else {
            return;
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
                        depth_slice: None,
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
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in full_output.textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(output_frame);
    }
}

// ---------------------------------------------------------------------------
// Native web notification renderer
// ---------------------------------------------------------------------------

/// Logical size of the clickable website-notification window. The card itself
/// fills this small top-level window, so the rest of the page keeps receiving
/// pointer input.
pub const WEB_NOTIFICATION_WIDTH: f32 = 368.0;
pub const WEB_NOTIFICATION_HEIGHT: f32 = 122.0;

/// Transparent always-on-top window used for a single website notification.
/// Unlike the download toast, this surface is intentionally hit-testable so a
/// click can activate the source tab and fire the page's `click` handler.
pub struct WebNotificationRenderer {
    surface: wgpu::Surface<'static>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    max_texture_dim: u32,
}

impl WebNotificationRenderer {
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
                tracing::error!("Failed to create web notification surface: {e}");
                return None;
            }
        };

        let size = window.inner_size();
        let caps = surface.get_capabilities(adapter);
        let Some(format) = preferred_surface_format(&caps.formats) else {
            tracing::error!("Web notification adapter reported no surface formats");
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
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let egui_ctx = egui::Context::default();
        // The notification card draws the same Material Symbols glyphs as the
        // rail and needs its own font registration.
        register_material_icon_font(&egui_ctx);
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

    /// Paint the notification card and return a click/close action when the
    /// user interacts with it.
    pub fn render(&mut self, window: &winit::window::Window, state: &ChromeState) -> Vec<UiAction> {
        self.resize(window.inner_size());
        state.apply_visuals(&self.egui_ctx);
        let palette = state.palette();
        let Some(toast) = state.web_notification.as_ref() else {
            return Vec::new();
        };
        let notification = toast.notification.clone();
        self.egui_ctx
            .request_repaint_after(toast.duration.saturating_sub(toast.created_at.elapsed()));

        let raw_input = self.egui_state.take_egui_input(window);
        let egui_ctx = self.egui_ctx.clone();
        let mut actions = Vec::new();
        let mut close_clicked = false;
        let mut card_clicked = false;
        let mut full_output = egui_ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::no_frame()
                .frame(Frame::NONE.fill(Color32::TRANSPARENT))
                .show(ui, |ui| {
                    let frame = Frame::NONE
                        .fill(palette.palette_bg)
                        .stroke(Stroke::new(1.0_f32, palette.rail_accent))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            // Fill the whole small window so its transparent
                            // margin cannot intercept page clicks.
                            ui.set_min_size(ui.available_size());
                            ui.horizontal(|ui| {
                                let (icon_rect, _) =
                                    ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                                paint_rail_icon(
                                    ui,
                                    egui::Rect::from_center_size(
                                        icon_rect.center(),
                                        vec2(15.0, 15.0),
                                    ),
                                    RailIcon::Notifications,
                                    palette.rail_accent,
                                );
                                ui.add_space(6.0);
                                let title = if notification.title.trim().is_empty() {
                                    "Notification"
                                } else {
                                    notification.title.trim()
                                };
                                ui.label(
                                    RichText::new(truncate_title(title, 52))
                                        .size(12.0)
                                        .strong()
                                        .color(palette.text_strong),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        close_clicked = compact_icon_button(
                                            ui,
                                            RailIcon::Close,
                                            "Dismiss notification",
                                            &palette,
                                            false,
                                        )
                                        .clicked();
                                    },
                                );
                            });
                            if !notification.body.is_empty() {
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(truncate_title(&notification.body, 150))
                                        .size(10.5)
                                        .color(palette.text),
                                );
                            }
                            ui.add_space(3.0);
                            ui.label(
                                RichText::new(truncate_title(&notification.origin, 56))
                                    .size(9.5)
                                    .color(palette.palette_hint),
                            );
                        });
                    card_clicked = frame.response.interact(Sense::click()).clicked();
                });
        });

        if close_clicked {
            actions.push(UiAction::DismissWebNotification {
                tab_id: notification.tab_id,
                notification_id: notification.id.clone(),
            });
        } else if card_clicked {
            actions.push(UiAction::ActivateWebNotification {
                tab_id: notification.tab_id,
                notification_id: notification.id.clone(),
            });
        }

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        for (id, deltas) in full_output.textures_delta.set.drain() {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let Some(output_frame) = acquire_surface_frame(
            &self.surface,
            &self.device,
            &self.surface_config,
            "web notification",
        ) else {
            return actions;
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

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("web_notification"),
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
                    label: Some("web_notification_rpass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
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
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in full_output.textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(output_frame);
        actions
    }
}

/// Which corner of the CEF content area a page HUD strip belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageHudSide {
    Left,
    Right,
}

/// Transparent native window used for always-visible page controls and status.
///
/// The CEF child view sits above the main wgpu surface, so these small strips
/// need their own top-level windows. They are sized to the controls rather than
/// the whole page, leaving all other pointer input to the browser.
pub struct PageHudRenderer {
    surface: wgpu::Surface<'static>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    max_texture_dim: u32,
}

impl PageHudRenderer {
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
                tracing::error!("Failed to create page HUD surface: {e}");
                return None;
            }
        };

        let size = window.inner_size();
        let caps = surface.get_capabilities(adapter);
        let Some(format) = preferred_surface_format(&caps.formats) else {
            tracing::error!("Page HUD adapter reported no surface formats");
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
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let egui_ctx = egui::Context::default();
        register_material_icon_font(&egui_ctx);
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

    /// Paint the bottom-left controls or bottom-right status strip.
    pub fn render(
        &mut self,
        window: &winit::window::Window,
        state: &mut ChromeState,
    ) -> Vec<UiAction> {
        let mut actions = Vec::new();
        self.resize(window.inner_size());
        state.apply_visuals(&self.egui_ctx);
        let palette = state.palette();
        let raw_input = self.egui_state.take_egui_input(window);
        let active_tab_id = state.active_tab_id;
        let blocked_requests = state
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == active_tab_id)
            .map_or(0, |tab| tab.blocked_requests);
        let ram_mb = state.ram_mb;
        let egui_ctx = self.egui_ctx.clone();
        let mut full_output = egui_ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::no_frame()
                .frame(Frame::NONE.fill(Color32::TRANSPARENT))
                .show(ui, |ui| {
                    ui.set_height(PAGE_HUD_HEIGHT);
                    Frame::NONE
                        .fill(palette.menu_bg)
                        .stroke(Stroke::new(0.0_f32, Color32::TRANSPARENT))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(8, PAGE_HUD_VERTICAL_MARGIN))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let command = ChromeRenderer::rail_icon_button_with_inset(
                                    ui,
                                    vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                    RailIcon::Search,
                                    &palette,
                                    PAGE_HUD_ICON_INSET,
                                );
                                if command.clicked() {
                                    state.open_command_palette();
                                }
                                command.on_hover_text("Command palette (Cmd/Ctrl+K)");

                                let devtools = ChromeRenderer::rail_icon_button_with_inset(
                                    ui,
                                    vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                    RailIcon::Code,
                                    &palette,
                                    PAGE_HUD_ICON_INSET,
                                );
                                if devtools.clicked() && active_tab_id.is_some() {
                                    if let Some(tab_id) = active_tab_id {
                                        actions.push(UiAction::OpenDevTools { tab_id });
                                    }
                                }
                                devtools.on_hover_text("DevTools (F12)");

                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let ram = ChromeRenderer::rail_icon_button_with_inset(
                                        ui,
                                        vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                        RailIcon::Chip,
                                        &palette,
                                        PAGE_HUD_ICON_INSET,
                                    );
                                    ram.on_hover_text(format!("RAM {ram_mb} MB"));
                                    ui.label(
                                        RichText::new(format!("{ram_mb}"))
                                            .size(9.0)
                                            .color(palette.text_dim),
                                    );

                                    let shield = ChromeRenderer::rail_icon_button_with_inset(
                                        ui,
                                        vec2(PAGE_HUD_BUTTON_SIZE, PAGE_HUD_BUTTON_SIZE),
                                        RailIcon::Shield,
                                        &palette,
                                        PAGE_HUD_ICON_INSET,
                                    );
                                    shield.on_hover_text(format!("{blocked_requests} ads blocked"));
                                    ui.label(
                                        RichText::new(blocked_requests.to_string())
                                            .size(9.0)
                                            .color(palette.shield_ok),
                                    );
                                });
                            });
                        })
                });
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        for (id, deltas) in full_output.textures_delta.set.drain() {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let Some(output_frame) = acquire_surface_frame(
            &self.surface,
            &self.device,
            &self.surface_config,
            "page HUD",
        ) else {
            return actions;
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

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("page_hud_overlay"),
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
                    label: Some("page_hud_overlay_rpass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
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
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &tris, &screen_descriptor);
        }

        for id in full_output.textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(output_frame);
        actions
    }
}

/// Draw one of the rail's icons inside `rect` using the registered Material
/// Symbols font. Font glyphs stay crisp at any DPI and give the rail a
/// consistent, familiar icon language.
fn rail_icon_glyph(icon: RailIcon) -> &'static str {
    use egui_material_icons::icons::{
        ICON_ADD, ICON_BEDTIME, ICON_BOOK, ICON_BOOKMARK_ADD, ICON_CHECK, ICON_CHEVRON_LEFT,
        ICON_CHEVRON_RIGHT, ICON_CLOSE, ICON_CODE, ICON_CONTENT_COPY, ICON_DELETE,
        ICON_DESKTOP_WINDOWS, ICON_DOWNLOAD, ICON_EDIT, ICON_FOLDER_OPEN, ICON_MEMORY,
        ICON_MUSIC_NOTE, ICON_NEW_WINDOW, ICON_NOTIFICATIONS, ICON_OPEN_IN_NEW, ICON_PALETTE,
        ICON_PAUSE, ICON_PIN, ICON_PLAY_ARROW, ICON_PRINT, ICON_PUBLIC, ICON_PUSH_PIN,
        ICON_REFRESH, ICON_ROCKET, ICON_SCHOOL, ICON_SEARCH, ICON_SETTINGS, ICON_SHIELD, ICON_STAR,
        ICON_UPLOAD, ICON_VIEW_SIDEBAR, ICON_VISIBILITY_OFF, ICON_WORK,
    };

    match icon {
        RailIcon::Plus => ICON_ADD.into(),
        RailIcon::Search => ICON_SEARCH.into(),
        RailIcon::Code => ICON_CODE.into(),
        RailIcon::ChevronLeft => ICON_CHEVRON_LEFT.into(),
        RailIcon::ChevronRight => ICON_CHEVRON_RIGHT.into(),
        RailIcon::Moon => ICON_BEDTIME.into(),
        RailIcon::Shield => ICON_SHIELD.into(),
        RailIcon::Chip => ICON_MEMORY.into(),
        RailIcon::Private => ICON_VISIBILITY_OFF.into(),
        RailIcon::Globe => ICON_PUBLIC.into(),
        RailIcon::DisplayScreen => ICON_DESKTOP_WINDOWS.into(),
        RailIcon::DisplayWindow => ICON_NEW_WINDOW.into(),
        RailIcon::Close => ICON_CLOSE.into(),
        RailIcon::Pin => ICON_PUSH_PIN.into(),
        RailIcon::PinOff => ICON_PIN.into(),
        RailIcon::Copy => ICON_CONTENT_COPY.into(),
        RailIcon::Sidebar => ICON_VIEW_SIDEBAR.into(),
        RailIcon::Settings => ICON_SETTINGS.into(),
        RailIcon::Download => ICON_DOWNLOAD.into(),
        RailIcon::Upload => ICON_UPLOAD.into(),
        RailIcon::Print => ICON_PRINT.into(),
        RailIcon::Pause => ICON_PAUSE.into(),
        RailIcon::Play => ICON_PLAY_ARROW.into(),
        RailIcon::Retry => ICON_REFRESH.into(),
        RailIcon::FolderOpen => ICON_FOLDER_OPEN.into(),
        RailIcon::Check => ICON_CHECK.into(),
        RailIcon::Delete => ICON_DELETE.into(),
        RailIcon::Star => ICON_STAR.into(),
        RailIcon::Edit => ICON_EDIT.into(),
        RailIcon::BookmarkAdd => ICON_BOOKMARK_ADD.into(),
        RailIcon::OpenInNew => ICON_OPEN_IN_NEW.into(),
        RailIcon::Notifications => ICON_NOTIFICATIONS.into(),
        RailIcon::WorkspaceWork => ICON_WORK.into(),
        RailIcon::WorkspaceCode => ICON_CODE.into(),
        RailIcon::WorkspaceStudy => ICON_SCHOOL.into(),
        RailIcon::WorkspaceLaunch => ICON_ROCKET.into(),
        RailIcon::WorkspaceDesign => ICON_PALETTE.into(),
        RailIcon::WorkspaceMusic => ICON_MUSIC_NOTE.into(),
        RailIcon::WorkspaceReading => ICON_BOOK.into(),
        RailIcon::WorkspaceWeb => ICON_PUBLIC.into(),
    }
}

/// Register the Material Symbols fallback used by every icon surface, including
/// the standalone website-notification window. Without this, private-use
/// glyphs such as the close icon render as tofu on a fresh `egui::Context`.
fn register_material_icon_font(ctx: &egui::Context) {
    egui_material_icons::initialize(ctx);
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

/// Closed ten-point star used for the bookmark toggle. The pinned Material
/// Symbols font only ships a filled star, so the unbookmarked state is drawn as
/// a native outline instead of relying on a missing glyph.
fn bookmark_star_points(rect: egui::Rect) -> Vec<egui::Pos2> {
    let center = rect.center();
    let outer = rect.width().min(rect.height()) * 0.5;
    let inner = outer * 0.44;
    let mut points = Vec::with_capacity(11);
    for index in 0..10 {
        let angle = -std::f32::consts::FRAC_PI_2 + (index as f32) * std::f32::consts::PI / 5.0;
        let radius = if index % 2 == 0 { outer } else { inner };
        points.push(pos2(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        ));
    }
    points.push(points[0]);
    points
}

fn paint_bookmark_toggle(ui: &Ui, rect: egui::Rect, bookmarked: bool, color: Color32) {
    if bookmarked {
        paint_rail_icon(ui, rect, RailIcon::Star, color);
    } else {
        ui.painter().add(egui::Shape::line(
            bookmark_star_points(rect),
            Stroke::new(1.25_f32, color),
        ));
    }
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
        .corner_radius(CornerRadius::same(8))
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
    let (rect, response) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::click());
    let color = if prominent || response.hovered() {
        palette.text_strong
    } else {
        palette.text
    };
    let icon_rect = egui::Rect::from_center_size(rect.center(), vec2(14.0, 14.0));
    paint_rail_icon(ui, icon_rect, icon, color);
    response.on_hover_text(label)
}

/// A tooltip for the find bar that does not use egui's default popup frame.
///
/// The find bar lives in a transparent overlay above the CEF page. egui's
/// `Frame::popup` applies `visuals.popup_shadow`, which reads as a dark box
/// around short tooltips when the snippet drawer is closed. Reusing the flat
/// rail-tooltip style keeps the overlay visually clean without changing
/// tooltips elsewhere in the browser.
fn flat_find_tooltip(
    response: Response,
    text: impl Into<egui::WidgetText>,
    palette: &ChromePalette,
) -> Response {
    let mut tooltip = egui::Tooltip::for_enabled(&response);
    tooltip.popup = tooltip.popup.frame(
        Frame::NONE
            .fill(palette.tooltip_bg)
            .stroke(Stroke::new(1.0_f32, palette.tooltip_border))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(8, 5)),
    );
    tooltip.show(|ui| {
        let text: egui::WidgetText = text.into();
        ui.label(text.color(palette.tooltip_text).size(11.0));
    });
    response
}

/// Icon button variant for the find bar with a shadowless tooltip.
fn compact_find_icon_button(
    ui: &mut Ui,
    icon: RailIcon,
    label: &'static str,
    palette: &ChromePalette,
    prominent: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::click());
    let color = if prominent || response.hovered() {
        palette.text_strong
    } else {
        palette.text
    };
    let icon_rect = egui::Rect::from_center_size(rect.center(), vec2(14.0, 14.0));
    paint_rail_icon(ui, icon_rect, icon, color);
    flat_find_tooltip(response, label, palette)
}

/// Stable colours for multi-term find results. The renderer assigns the same
/// term index to every match of one query term, so the drawer uses that index
/// to keep its label colour in sync with the page highlight.
fn find_term_color(index: usize) -> Color32 {
    const COLORS: [Color32; 5] = [
        Color32::from_rgb(255, 205, 86),
        Color32::from_rgb(126, 231, 135),
        Color32::from_rgb(129, 190, 255),
        Color32::from_rgb(255, 150, 198),
        Color32::from_rgb(196, 168, 255),
    ];
    COLORS[index % COLORS.len()]
}

/// Draw a cached favicon in the find results list without borrowing a full
/// `ChromeRenderer` from the overlay renderer.
fn paint_find_favicon(
    ui: &mut Ui,
    tab: &TabInfo,
    textures: &HashMap<String, egui::TextureHandle>,
    palette: &ChromePalette,
) {
    let rect = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover()).0;
    if !ui.is_rect_visible(rect) {
        return;
    }
    if let Some(favicon) = &tab.favicon {
        if let Some(texture) = textures.get(&favicon.url) {
            let uv = egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            ui.painter().image(texture.id(), rect, uv, Color32::WHITE);
            return;
        }
    }
    paint_rail_icon(ui, rect, RailIcon::Globe, palette.text_dim);
}

/// Apply the drawer's optional substring filter to a tab's rich snippets.
fn filtered_find_snippets<'a>(
    tab: Option<&'a TabInfo>,
    filter: &str,
    enabled: bool,
) -> Vec<&'a FindSnippet> {
    let Some(tab) = tab else {
        return Vec::new();
    };
    let needle = if enabled { filter.trim() } else { "" };
    tab.find_snippets
        .iter()
        .filter(|snippet| {
            needle.is_empty()
                || snippet.line.to_lowercase().contains(&needle.to_lowercase())
                || snippet
                    .heading
                    .to_lowercase()
                    .contains(&needle.to_lowercase())
                || snippet.href.to_lowercase().contains(&needle.to_lowercase())
        })
        .collect()
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

/// Logical geometry for the floating find bar inside its overlay viewport.
///
/// The overlay tracks the main window across displays and DPI changes, so the
/// bar is anchored from the *current* viewport on every frame. Width and x are
/// clamped to keep the card inside even a very small or freshly resized window.
/// Returns `(width, x, y)`.
pub fn find_bar_geometry(viewport: egui::Rect) -> (f32, f32, f32) {
    let available_width = (viewport.width() - 16.0).max(1.0);
    let width = 640.0_f32.min(available_width);
    let x = ((viewport.width() - width) * 0.5).max(8.0);
    let y = 14.0_f32.min((viewport.height() - 80.0).max(4.0));
    (width, x, y)
}

/// Tooltips are labels for the icon-only collapsed rail. Expanded controls
/// already draw visible text, so expanded-mode tooltips are disabled.
pub fn rail_tooltip_enabled(expanded: bool) -> bool {
    !expanded
}

/// Maximum tooltip width and minimum card height for each rail mode. Actual
/// tooltip cards are sized from their wrapped text with `tooltip_size_for_text`.
pub fn rail_tooltip_metrics(expanded: bool) -> (f32, f32) {
    if expanded {
        (TOOLTIP_MAX_WIDTH, TOOLTIP_HEIGHT_EXPANDED)
    } else {
        (TOOLTIP_MAX_WIDTH, TOOLTIP_HEIGHT_COLLAPSED)
    }
}

/// Lay out tooltip text with the same wrapping rules in both the chrome frame
/// that sizes the overlay window and the overlay frame that paints it.
fn with_tooltip_galley<R>(
    ctx: &egui::Context,
    text: &str,
    use_galley: impl FnOnce(Arc<egui::text::Galley>) -> R,
) -> R {
    ctx.fonts_mut(|fonts| {
        let mut job = egui::text::LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(TOOLTIP_FONT_SIZE),
            Color32::PLACEHOLDER,
            TOOLTIP_MAX_WIDTH - 2.0 * TOOLTIP_TEXT_MARGIN_X,
        );
        // Long titles and URLs often have no spaces; break them rather than
        // letting a single row run outside the card.
        job.wrap.break_anywhere = true;
        job.wrap.max_rows = TOOLTIP_MAX_ROWS;
        use_galley(fonts.layout_job(job))
    })
}

/// Size a tooltip card from its wrapped text so long labels never run outside
/// the overlay window. Width shrinks to the longest row and height grows with
/// the row count, within a bounded maximum width and row count.
pub fn tooltip_size_for_text(ctx: &egui::Context, text: &str, expanded: bool) -> (f32, f32) {
    let (_, min_height) = rail_tooltip_metrics(expanded);
    with_tooltip_galley(ctx, text, |galley| {
        let text_width = galley.rect.width().clamp(
            TOOLTIP_MIN_WIDTH,
            TOOLTIP_MAX_WIDTH - 2.0 * TOOLTIP_TEXT_MARGIN_X,
        );
        let width = (text_width + 2.0 * TOOLTIP_TEXT_MARGIN_X).min(TOOLTIP_MAX_WIDTH);
        let height = (galley.rect.height() + 2.0 * TOOLTIP_TEXT_MARGIN_Y).max(min_height);
        (width, height)
    })
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

/// Number of rows in the context menu, including the dynamic workspace list.
pub fn rail_menu_item_count(state: &ChromeState, tab_id: u64) -> usize {
    const BASE_ITEMS: usize = 10;
    let tab = state.tabs.iter().find(|tab| tab.id == tab_id);
    if tab.is_some_and(|tab| tab.is_pinned) {
        return BASE_ITEMS + 1;
    }
    let current_workspace_id = tab
        .map(|tab| tab.workspace_id)
        .unwrap_or(state.active_workspace_id);
    let other_workspaces = state
        .workspaces
        .iter()
        .filter(|workspace| workspace.id != current_workspace_id)
        .count();
    BASE_ITEMS + 1 + other_workspaces.max(1)
}

/// Card size for the rail tab context menu. It grows when more workspaces are
/// available as move targets.
pub fn rail_menu_metrics(state: &ChromeState, tab_id: u64) -> (f32, f32) {
    let item_count = rail_menu_item_count(state, tab_id);
    (
        RAIL_MENU_WIDTH,
        item_count as f32 * RAIL_MENU_ROW_HEIGHT + RAIL_MENU_PADDING * 2.0,
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
    let has_closable_other_tabs = workspace_tabs
        .iter()
        .any(|tab| tab.id != tab_id && !tab.is_pinned);
    let pin_label = if is_pinned { "Unpin tab" } else { "Pin tab" };
    let item = |label: &str, action, enabled| RailMenuItem {
        label: label.to_string(),
        action,
        enabled,
    };
    let mut items = vec![
        item("New tab", RailMenuAction::NewTab, true),
        item("Duplicate tab", RailMenuAction::DuplicateTab, true),
        item(pin_label, RailMenuAction::TogglePinTab, true),
        item("Copy URL", RailMenuAction::CopyUrl, true),
        item("Reload tab", RailMenuAction::Reload, true),
        item("Close tab", RailMenuAction::CloseTab, true),
        item(
            "Close other tabs",
            RailMenuAction::CloseOtherTabs,
            has_closable_other_tabs,
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
    ];

    let current_workspace_id = state
        .tabs
        .iter()
        .find(|tab| tab.id == tab_id)
        .map(|tab| tab.workspace_id)
        .unwrap_or(state.active_workspace_id);

    // Pinned tabs stay in their workspace. Offering a move would break the
    // pinned section invariant, so the heading explains why no targets appear.
    if is_pinned {
        items.push(item(
            "Move tab to (unpin first)",
            RailMenuAction::MoveTabToHeader,
            false,
        ));
        return items;
    }
    if Workspace::is_private_id(current_workspace_id) {
        items.push(item(
            "Incognito tabs stay private",
            RailMenuAction::MoveTabToHeader,
            false,
        ));
        return items;
    }

    items.push(item("MOVE TAB TO", RailMenuAction::MoveTabToHeader, false));
    let mut targets: Vec<&WorkspaceInfo> = state
        .workspaces
        .iter()
        .filter(|workspace| {
            workspace.id != current_workspace_id && !Workspace::is_private_id(workspace.id)
        })
        .collect();
    targets.sort_by_key(|workspace| workspace.name.clone());
    if targets.is_empty() {
        items.push(item(
            "No other workspaces",
            RailMenuAction::MoveTabToHeader,
            false,
        ));
    } else {
        for workspace in targets {
            items.push(RailMenuItem {
                label: format!("Move to {}", workspace.name),
                action: RailMenuAction::MoveTabToWorkspace {
                    workspace_id: workspace.id,
                },
                enabled: true,
            });
        }
    }
    items
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
        RailMenuAction::MoveTabToHeader => None,
        RailMenuAction::MoveTabToWorkspace { workspace_id } => Some(UiAction::MoveTabToWorkspace {
            tab_id,
            workspace_id,
        }),
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

/// Place a HUD tooltip above its button, clamped inside the window. HUD
/// tooltips use the native overlay window so they are never covered by the
/// CEF child view.
pub fn bottom_hud_tooltip_anchor(
    button: egui::Rect,
    screen: egui::Rect,
    tooltip_width: f32,
    tooltip_height: f32,
) -> egui::Pos2 {
    let min_x = screen.left() + 6.0;
    let max_x = (screen.right() - tooltip_width - 6.0).max(min_x);
    let x = (button.center().x - tooltip_width * 0.5).clamp(min_x, max_x);
    let min_y = screen.top() + 6.0;
    let max_y = (screen.bottom() - tooltip_height - 6.0).max(min_y);
    let y = (button.top() - tooltip_height - 6.0).clamp(min_y, max_y);
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
    use egui_material_icons::icons::{
        ICON_ADD, ICON_BEDTIME, ICON_BOOKMARK_ADD, ICON_BOOKMARK_MANAGER, ICON_DOWNLOAD,
        ICON_HISTORY, ICON_OPEN_IN_NEW, ICON_PRINT, ICON_PUSH_PIN, ICON_SETTINGS, ICON_TAB,
        ICON_TERMINAL, ICON_UPLOAD, ICON_VIEW_SIDEBAR, ICON_VISIBILITY_OFF,
    };

    let trimmed = query.trim();
    let mut items = Vec::new();

    let tab_item = |tab: &TabInfo| CommandPaletteItem {
        icon: if Workspace::is_private_id(tab.workspace_id) {
            ICON_VISIBILITY_OFF.into()
        } else if tab.is_pinned {
            ICON_PUSH_PIN.into()
        } else if tab.display_state == TabDisplayState::Hibernated {
            ICON_BEDTIME.into()
        } else {
            ICON_TAB.into()
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
                icon: ICON_PUSH_PIN.into(),
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

    let bookmark_item = || {
        active_tab_id.map(|_| CommandPaletteItem {
            icon: ICON_BOOKMARK_ADD.into(),
            label: "Bookmark this page".to_string(),
            subtitle: "Save or remove the active page bookmark".to_string(),
            action: CommandAction::BookmarkActiveTab,
        })
    };
    let bookmark_manager_item = || CommandPaletteItem {
        icon: ICON_BOOKMARK_MANAGER.into(),
        label: "Open bookmark manager (Cmd/Ctrl+Shift+B)".to_string(),
        subtitle: "Search, edit, move, and delete saved pages".to_string(),
        action: CommandAction::OpenBookmarkManager,
    };
    let browser_import_item = || CommandPaletteItem {
        icon: ICON_UPLOAD.into(),
        label: "Import browser data".to_string(),
        subtitle: "Bring bookmarks and history from Chrome, Edge, Firefox, or Safari".to_string(),
        action: CommandAction::OpenBrowserImport,
    };
    let history_item = || CommandPaletteItem {
        icon: ICON_HISTORY.into(),
        label: "Open history manager (Cmd/Ctrl+Y)".to_string(),
        subtitle: "Search, reopen, and remove visited pages".to_string(),
        action: CommandAction::OpenHistory,
    };
    let export_bookmarks_item = || CommandPaletteItem {
        icon: ICON_UPLOAD.into(),
        label: "Export bookmarks (HTML)".to_string(),
        subtitle: "Save a Netscape bookmark file other browsers can import".to_string(),
        action: CommandAction::ExportBookmarks,
    };
    let export_history_item = || CommandPaletteItem {
        icon: ICON_UPLOAD.into(),
        label: "Export history (CSV)".to_string(),
        subtitle: "Save all visited pages with Unix and UTC timestamps".to_string(),
        action: CommandAction::ExportHistory,
    };

    if trimmed.is_empty() {
        items.push(CommandPaletteItem {
            icon: ICON_ADD.into(),
            label: "New tab".to_string(),
            subtitle: "Open a fresh tab".to_string(),
            action: CommandAction::NewTab(None),
        });
        if let Some(item) = pin_item() {
            items.push(item);
        }
        items.push(CommandPaletteItem {
            icon: ICON_BEDTIME.into(),
            label: "Hibernate all background tabs".to_string(),
            subtitle: "Immediately close background browser views".to_string(),
            action: CommandAction::HibernateAll,
        });
        items.push(CommandPaletteItem {
            icon: ICON_VIEW_SIDEBAR.into(),
            label: "Toggle workspace rail".to_string(),
            subtitle: "Expand or collapse the left rail".to_string(),
            action: CommandAction::ToggleRail,
        });
        items.push(CommandPaletteItem {
            icon: ICON_SETTINGS.into(),
            label: "Open Settings".to_string(),
            subtitle: "Theme, search, privacy, and performance".to_string(),
            action: CommandAction::OpenSettings,
        });
        items.push(browser_import_item());
        items.push(CommandPaletteItem {
            icon: ICON_DOWNLOAD.into(),
            label: "Open Downloads".to_string(),
            subtitle: "Native parallel downloads and history".to_string(),
            action: CommandAction::OpenDownloads,
        });
        items.push(history_item());
        if active_tab_id.is_some() {
            items.push(CommandPaletteItem {
                icon: ICON_PRINT.into(),
                label: "Print active page (Cmd/Ctrl+P)".to_string(),
                subtitle: "Open the native print dialog".to_string(),
                action: CommandAction::Print,
            });
        }
        items.extend(tabs.iter().take(12).map(tab_item));
        if let Some(item) = bookmark_item() {
            items.push(item);
        }
        items.push(bookmark_manager_item());
        items.push(CommandPaletteItem {
            icon: ICON_VISIBILITY_OFF.into(),
            label: "New Incognito Window (Cmd/Ctrl+Shift+N)".to_string(),
            subtitle: "Private space with no history, cookies, or session writes".to_string(),
            action: CommandAction::NewPrivateWindow,
        });
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
            icon: ICON_OPEN_IN_NEW.into(),
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
        icon: ICON_ADD.into(),
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
                icon: ICON_TERMINAL.into(),
                label: "Open DevTools".to_string(),
                subtitle: "Inspect the active tab in a new window".to_string(),
                action: CommandAction::OpenDevTools,
            });
            let _ = tab_id;
        }
    }

    if needle.contains("print") && active_tab_id.is_some() {
        items.push(CommandPaletteItem {
            icon: ICON_PRINT.into(),
            label: "Print active page".to_string(),
            subtitle: "Open the native print dialog".to_string(),
            action: CommandAction::Print,
        });
    }

    if needle.contains("setting") || needle.contains("preference") {
        items.push(CommandPaletteItem {
            icon: ICON_SETTINGS.into(),
            label: "Open Settings".to_string(),
            subtitle: "Theme, search, privacy, and performance".to_string(),
            action: CommandAction::OpenSettings,
        });
    }

    if needle.contains("import") || needle.contains("browser data") {
        items.push(browser_import_item());
    }

    if needle.contains("download") {
        items.push(CommandPaletteItem {
            icon: ICON_DOWNLOAD.into(),
            label: "Open Downloads".to_string(),
            subtitle: "Native parallel downloads and history".to_string(),
            action: CommandAction::OpenDownloads,
        });
    }

    if needle.contains("history") {
        items.push(history_item());
    }

    if needle.contains("bookmark") || needle.contains("save page") {
        if let Some(item) = bookmark_item() {
            items.push(item);
        }
        items.push(bookmark_manager_item());
    }

    if needle.contains("export") {
        items.push(export_bookmarks_item());
        items.push(export_history_item());
    }

    if needle.contains("pin") {
        if let Some(item) = pin_item() {
            items.push(item);
        }
    }

    if needle.contains("incognito") || needle.contains("private") {
        items.push(CommandPaletteItem {
            icon: ICON_VISIBILITY_OFF.into(),
            label: "New Incognito Window (Cmd/Ctrl+Shift+N)".to_string(),
            subtitle: "Private space with no history, cookies, or session writes".to_string(),
            action: CommandAction::NewPrivateWindow,
        });
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
        CommandAction::NewPrivateWindow => actions.push(UiAction::NewPrivateWindow),
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
        CommandAction::Print => {
            if let Some(tab_id) = state.active_tab_id {
                actions.push(UiAction::Print { tab_id });
            }
        }
        CommandAction::OpenSettings => state.open_settings(),
        CommandAction::OpenBrowserImport => state.open_browser_import(),
        CommandAction::ExportBookmarks => actions.push(UiAction::ExportBrowserData {
            bookmarks: true,
            history: false,
        }),
        CommandAction::ExportHistory => actions.push(UiAction::ExportBrowserData {
            bookmarks: false,
            history: true,
        }),
        CommandAction::OpenDownloads => state.open_downloads(),
        CommandAction::OpenHistory => actions.push(UiAction::OpenHistory),
        CommandAction::OpenBookmarkManager => state.open_bookmark_manager(),
        CommandAction::BookmarkActiveTab => {
            if active_tab_is_private(state) {
                state.show_toast(
                    ToastKind::Error,
                    "Incognito",
                    "Bookmarks are unavailable in a private space",
                );
            } else if let Some((workspace_id, url)) = active_bookmark_key(state) {
                actions.push(UiAction::DeleteBookmark { workspace_id, url });
                state.show_toast(
                    ToastKind::Success,
                    "Bookmark removed",
                    "Removed from this workspace",
                );
            } else if let Some(tab_id) = state.active_tab_id {
                actions.push(UiAction::AddBookmark { tab_id });
                state.show_toast(
                    ToastKind::Success,
                    "Bookmark saved",
                    "Added to this workspace",
                );
            }
        }
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
// Bookmark manager helpers
// ---------------------------------------------------------------------------

/// Key of the bookmark for the active tab, if it has one. Bookmarks are keyed
/// by workspace and URL so new in-memory rows never need a database id.
pub fn active_bookmark_key(state: &ChromeState) -> Option<(u64, String)> {
    let tab = active_tab(state)?;
    if Workspace::is_private_id(tab.workspace_id) {
        return None;
    }
    state
        .bookmarks
        .iter()
        .any(|bookmark| bookmark.workspace_id == tab.workspace_id && bookmark.url == tab.url)
        .then(|| (tab.workspace_id, tab.url.clone()))
}

/// True when the selected tab belongs to the ephemeral incognito workspace.
pub fn active_tab_is_private(state: &ChromeState) -> bool {
    active_tab(state).is_some_and(|tab| Workspace::is_private_id(tab.workspace_id))
}

/// Shared lookup for helpers that need the active tab's metadata.
fn active_tab(state: &ChromeState) -> Option<&TabInfo> {
    state
        .tabs
        .iter()
        .find(|tab| Some(tab.id) == state.active_tab_id)
}

/// Filter bookmarks by title, URL, or folder without changing their order.
pub fn filter_bookmarks(bookmarks: &[BookmarkInfo], query: &str) -> Vec<BookmarkInfo> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return bookmarks.to_vec();
    }
    bookmarks
        .iter()
        .filter(|bookmark| {
            bookmark.title.to_lowercase().contains(&needle)
                || bookmark.url.to_lowercase().contains(&needle)
                || bookmark.folder.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.as_bytes().windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Return matching indices without cloning history rows. The native manager
/// uses this to lay out only the visible page of a large snapshot.
pub fn filter_history_indices(history: &[HistoryItem], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (0..history.len()).collect();
    }
    history
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            if needle.is_ascii() {
                contains_ascii_case_insensitive(&item.title, &needle)
                    || contains_ascii_case_insensitive(&item.url, &needle)
            } else {
                item.title.to_lowercase().contains(&needle)
                    || item.url.to_lowercase().contains(&needle)
            }
        })
        .map(|(index, _)| index)
        .collect()
}

/// Filter history entries by title or URL without changing recency order.
pub fn filter_history(history: &[HistoryItem], query: &str) -> Vec<HistoryItem> {
    filter_history_indices(history, query)
        .into_iter()
        .map(|index| history[index].clone())
        .collect()
}

/// Compact relative timestamp used by the native history manager. `now` is
/// passed in so the formatting is deterministic in unit tests.
pub fn format_history_time(last_visited: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(last_visited);
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3_600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else if elapsed < 172_800 {
        "yesterday".to_string()
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

/// Normalize a bookmark URL without silently turning plain text into a search.
pub fn normalize_bookmark_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if url::Url::parse(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    if !trimmed.contains(' ') && trimmed.contains('.') {
        let candidate = format!("https://{trimmed}");
        if url::Url::parse(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
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
    use egui_material_icons::icons::ICON_VISIBILITY_OFF;
    use std::path::PathBuf;

    use super::{
        active_bookmark_key, advance_command_selection, apply_command_item, bookmark_star_points,
        bottom_hud_tooltip_anchor, clamp_command_selection, command_palette_click_outside,
        command_palette_items, download_engine_label, download_eta_label, download_rail_badge,
        download_row_uses_progress_bar, download_state_label, downloads_bubble_anchor,
        downloads_bubble_size, effective_theme, filter_bookmarks, filter_history,
        filter_history_indices, find_bar_geometry, focus_request_after_render,
        format_download_bytes, format_history_time, loading_spinner_angle, loading_spinner_point,
        normalize_bookmark_url, page_tooltip_anchor, palette_row_fill, preferred_surface_format,
        rail_menu_anchor, rail_menu_click_outside, rail_menu_items, rail_menu_metrics,
        rail_menu_ui_action, rail_status_icon_size, rail_tooltip_enabled, rail_tooltip_metrics,
        register_material_icon_font, resolve_omnibox, resolve_omnibox_with_search,
        tab_close_center, tab_status_center, tooltip_size_for_text, truncate_title,
        workspace_form_actions, BookmarkInfo, ChromePalette, ChromeRenderer, ChromeState,
        ChromeTheme, CommandAction, DownloadBadge, DownloadSummary, PermissionPrompt,
        PermissionPromptKind, PrintDialogState, RailMenuAction, SettingsTab, TabDisplayState,
        TabInfo, ToastKind, WorkspaceInfo, RAIL_STATUS_HEIGHT, RAIL_STATUS_ICON_COLLAPSED,
        RAIL_STATUS_ICON_EXPANDED, TOOLTIP_HEIGHT_COLLAPSED, TOOLTIP_HEIGHT_EXPANDED,
        TOOLTIP_MAX_WIDTH,
    };
    use crate::config::{
        BrowserPreferences, SearchEngine, ThemePreference, Workspace, WorkspaceColor,
        WorkspaceIcon, DEFAULT_HOME_URL, DEFAULT_SEARCH_URL_PREFIX, HISTORY_VISIBLE_PAGE_SIZE,
        RAIL_WIDTH_COLLAPSED, RAIL_WIDTH_EXPANDED,
    };
    use crate::events::{
        DownloadEngine, DownloadInfo, DownloadNotification, DownloadNotificationKind,
        DownloadState, PrintOptions, PrintPaperSize, UiAction, WebNotification,
    };
    use crate::storage::db::HistoryItem;
    use crate::storage::import::{BrowserProfile, BrowserSource, ImportReport};

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
            find_count: -1,
            find_active_ordinal: -1,
            find_pending: false,
            find_snippets: Vec::new(),
            find_error: String::new(),
        }
    }

    fn bookmark(id: i64, title: &str, url: &str, folder: &str) -> BookmarkInfo {
        BookmarkInfo {
            id,
            workspace_id: 1,
            url: url.to_string(),
            title: title.to_string(),
            folder: folder.to_string(),
        }
    }

    #[test]
    fn permission_prompt_is_modal_and_dismissal_does_not_persist() {
        let mut state = ChromeState::default();
        assert!(!state.has_modal_overlay());

        state.permission_prompt = Some(PermissionPrompt {
            kind: PermissionPromptKind::Media,
            request_id: 42,
            tab_id: 7,
            origin: "https://meet.example".to_string(),
            permissions: 3,
            label: "microphone and camera".to_string(),
            private: false,
        });
        assert!(state.has_modal_overlay());
        assert!(!state.is_toast_only());

        let mut actions = Vec::new();
        state.dismiss_permission_prompt(&mut actions);
        assert_eq!(
            actions,
            vec![UiAction::ResolveMediaAccessPermission {
                request_id: 42,
                granted: false,
                persist: false,
            }]
        );
    }

    #[test]
    fn print_dialog_is_modal_and_dismissal_cancels_the_job() {
        let mut state = ChromeState::default();
        let options = PrintOptions {
            printer: "Office".to_string(),
            paper: PrintPaperSize::A4,
            copies: 2,
            landscape: true,
            color: false,
            selection_only: false,
        };
        state.print_dialog = Some(PrintDialogState {
            tab_id: 7,
            has_selection: true,
            options: options.clone(),
            printers: vec!["Office".to_string()],
            default_printer: "Office".to_string(),
        });

        assert!(state.has_modal_overlay());
        assert!(!state.is_toast_only());

        let mut actions = Vec::new();
        state.dismiss_print_dialog(&mut actions);
        assert_eq!(
            actions,
            vec![UiAction::RespondPrintDialog {
                tab_id: 7,
                accept: false,
                options,
            }]
        );
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
    fn bookmark_search_matches_title_url_and_folder() {
        let items = vec![
            bookmark(1, "Rust docs", "https://doc.rust-lang.org", "Work"),
            bookmark(
                2,
                "Release notes",
                "https://example.com/releases",
                "Personal",
            ),
        ];
        assert_eq!(filter_bookmarks(&items, "").len(), 2);
        assert_eq!(filter_bookmarks(&items, "RUST")[0].id, 1);
        assert_eq!(filter_bookmarks(&items, "example.com")[0].id, 2);
        assert_eq!(filter_bookmarks(&items, "personal")[0].id, 2);
        assert!(filter_bookmarks(&items, "missing").is_empty());
    }

    #[test]
    fn bookmark_url_normalization_never_creates_a_search_url() {
        assert_eq!(
            normalize_bookmark_url("example.com/docs"),
            Some("https://example.com/docs".to_string())
        );
        assert_eq!(
            normalize_bookmark_url("  https://example.com  "),
            Some("https://example.com".to_string())
        );
        assert_eq!(normalize_bookmark_url("hello world"), None);
        assert_eq!(normalize_bookmark_url("   "), None);
    }

    #[test]
    fn download_notifications_map_to_success_and_error_toasts() {
        let mut state = ChromeState::default();
        state.show_download_notification(DownloadNotification {
            download_id: 1,
            file_name: "archive.zip".to_string(),
            kind: DownloadNotificationKind::Completed,
        });
        let toast = state.toast.clone().expect("completion toast");
        assert_eq!(toast.kind, ToastKind::Success);
        assert_eq!(toast.title, "Download complete");
        assert!(toast.message.contains("archive.zip"));

        state.show_download_notification(DownloadNotification {
            download_id: 2,
            file_name: "broken.iso".to_string(),
            kind: DownloadNotificationKind::Failed,
        });
        // A second outcome waits its turn instead of replacing the first.
        let toast = state.toast.clone().expect("visible completion toast");
        assert_eq!(toast.title, "Download complete");
        assert_eq!(state.pending_toasts.len(), 1);

        if let Some(visible) = state.toast.as_mut() {
            visible.created_at -= std::time::Duration::from_secs(5);
        }
        state.expire_toast();
        let toast = state.toast.expect("failure toast");
        assert_eq!(toast.kind, ToastKind::Error);
        assert_eq!(toast.title, "Download failed");
        assert!(toast.message.contains("broken.iso"));
        assert!(state.pending_toasts.is_empty());
    }

    #[test]
    fn copy_url_targets_the_active_tab_then_the_omnibox() {
        let mut state = ChromeState::default();
        assert_eq!(state.active_url(), None);

        state.tabs = vec![
            tab(1, "One", "https://one.example/"),
            tab(2, "Two", "https://two.example/"),
        ];
        state.omnibox_text = "https://one.example/".to_string();
        assert_eq!(state.active_url().as_deref(), Some("https://one.example/"));

        state.active_tab_id = Some(2);
        assert_eq!(state.active_url().as_deref(), Some("https://two.example/"));

        // Before the first tab exists, the mirrored address text still copies.
        state.tabs.clear();
        state.omnibox_text = "https://fallback.example/".to_string();
        assert_eq!(
            state.active_url().as_deref(),
            Some("https://fallback.example/")
        );

        state.omnibox_text.clear();
        assert_eq!(state.active_url(), None);
    }

    #[test]
    fn copy_url_shortcut_writes_the_active_url_and_confirms_once() {
        let mut state = ChromeState::default();
        state.tabs = vec![tab(3, "Docs", "https://example.com/docs")];
        state.active_tab_id = Some(3);
        state.request_copy_url();
        assert!(state.copy_url_requested);

        let ctx = egui::Context::default();
        let input = egui::RawInput::default();
        let mut output = ctx.run_ui(input, |ui| {
            ChromeRenderer::serve_copy_url_request(ui.ctx(), &mut state);
        });
        output.textures_delta.clear();

        // The request is served exactly once, even though the shell redraws
        // every loop iteration.
        assert!(!state.copy_url_requested);
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(
                command,
                egui::OutputCommand::CopyText(text) if text == "https://example.com/docs"
            )
        }));
        let toast = state.toast.expect("copy confirmation");
        assert_eq!(toast.kind, ToastKind::Success);
        assert_eq!(toast.title, "Copied");
    }

    #[test]
    fn copy_url_shortcut_without_a_page_does_nothing() {
        let mut state = ChromeState::default();
        state.request_copy_url();

        let ctx = egui::Context::default();
        let input = egui::RawInput::default();
        let mut output = ctx.run_ui(input, |ui| {
            ChromeRenderer::serve_copy_url_request(ui.ctx(), &mut state);
        });
        output.textures_delta.clear();

        assert!(!state.copy_url_requested);
        assert!(output.platform_output.commands.is_empty());
        assert!(state.toast.is_none());
    }

    #[test]
    fn toasts_queue_while_a_modal_owns_the_overlay() {
        let mut state = ChromeState::default();
        state.open_settings();
        state.show_toast(ToastKind::Success, "Copied", "Link copied to clipboard");
        assert!(state.toast.is_none());
        assert_eq!(state.pending_toasts.len(), 1);

        state.close_settings();
        state.promote_pending_toast();
        let toast = state.toast.expect("promoted toast");
        assert_eq!(toast.title, "Copied");
        assert!(toast.created_at.elapsed() < std::time::Duration::from_secs(1));
    }

    fn web_notification(id: &str, tab_id: u64, require_interaction: bool) -> WebNotification {
        WebNotification {
            id: id.to_string(),
            tab_id,
            origin: "https://example.com".to_string(),
            title: "Build finished".to_string(),
            body: "The nightly artifact is ready".to_string(),
            tag: String::new(),
            require_interaction,
        }
    }

    #[test]
    fn web_notifications_queue_behind_modals_and_promote_after_dismissal() {
        let mut state = ChromeState::default();
        state.open_settings();
        state.show_web_notification(web_notification("kn-1", 7, false));
        assert!(state.web_notification.is_none());
        assert_eq!(state.pending_web_notifications.len(), 1);

        state.close_settings();
        state.promote_pending_web_notification();
        let toast = state.web_notification.expect("promoted notification");
        assert_eq!(toast.notification.id, "kn-1");
        assert!(toast.created_at.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn web_notifications_replace_by_id_and_require_interaction_stays_visible() {
        let mut state = ChromeState::default();
        state.show_web_notification(web_notification("kn-2", 3, true));
        assert!(state.web_notification.is_some());
        state.show_web_notification(web_notification("kn-3", 3, false));
        assert_eq!(state.pending_web_notifications.len(), 1);

        state.remove_web_notification("kn-2");
        assert_eq!(
            state
                .web_notification
                .as_ref()
                .map(|toast| toast.notification.id.as_str()),
            Some("kn-3")
        );
        let taken = state.take_web_notification().expect("taken notification");
        assert_eq!(taken.id, "kn-3");
        assert!(state.web_notification.is_none());
    }

    #[test]
    fn notification_permission_dismissal_does_not_persist() {
        let mut state = ChromeState::default();
        state.permission_prompt = Some(PermissionPrompt {
            kind: PermissionPromptKind::Notifications,
            request_id: 9,
            tab_id: 4,
            origin: "https://chat.example".to_string(),
            permissions: 0,
            label: "notifications".to_string(),
            private: false,
        });
        assert!(state.has_modal_overlay());

        let mut actions = Vec::new();
        state.dismiss_permission_prompt(&mut actions);
        assert_eq!(
            actions,
            vec![UiAction::ResolveNotificationPermission {
                request_id: 9,
                granted: false,
                persist: false,
            }]
        );
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
        assert_eq!(width, TOOLTIP_MAX_WIDTH);
        assert_eq!(height, TOOLTIP_HEIGHT_COLLAPSED);
        assert!((240.0..=320.0).contains(&width));
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
    fn long_tooltips_wrap_and_grow_instead_of_overflowing() {
        // Font metrics are only available after the first egui pass.
        let ctx = egui::Context::default();
        let mut initial_frame = ctx.run_ui(egui::RawInput::default(), |_| {});
        initial_frame.textures_delta.clear();

        let short = tooltip_size_for_text(&ctx, "RAM 512 MB", false);
        let long = tooltip_size_for_text(
            &ctx,
            "Switch to a workspace with an extremely long custom name that definitely needs more than two rows in the collapsed rail tooltip overlay",
            false,
        );
        let multiline = tooltip_size_for_text(
            &ctx,
            "Secret title with a long descriptive prefix\nhttps://a-really-long.example.com/some/deep/path/that/keeps/going",
            false,
        );

        assert!(short.1 <= TOOLTIP_HEIGHT_COLLAPSED);
        assert!(long.1 > short.1, "wrapped label must make the card taller");
        assert!(
            multiline.1 > short.1,
            "explicit lines must make the card taller"
        );
        assert!(long.0 <= TOOLTIP_MAX_WIDTH);
        assert!(multiline.0 <= TOOLTIP_MAX_WIDTH);
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

        clear_textures(
            &ctx,
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    pos2(0.0, 0.0),
                    vec2(1280.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                egui::Panel::left("rail_address_field_test")
                    .exact_size(RAIL_WIDTH_EXPANDED)
                    .resizable(false)
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        let ctx = ui.ctx().clone();
                        let response = ChromeRenderer::build_rail_address_field(
                            ui,
                            &ctx,
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
    fn collapsed_status_actions_fit_inside_the_reserved_block() {
        // Shield and RAM moved to the page HUD; the rail now holds only
        // expand, hibernate, Settings, and Downloads.
        let used_height = 5.0 + 30.0 + 4.0 + 30.0 + 4.0 + 30.0 + 4.0 + 30.0;
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

    fn clear_textures(ctx: &egui::Context, raw: egui::RawInput, run: impl FnMut(&mut egui::Ui)) {
        let mut output = ctx.run_ui(raw, run);
        output.textures_delta.clear();
    }

    #[test]
    fn bottom_hud_tooltip_anchor_sits_above_the_button_and_inside_the_window() {
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let button = egui::Rect::from_min_size(pos2(230.0, 760.0), vec2(30.0, 30.0));
        let anchor =
            bottom_hud_tooltip_anchor(button, screen, TOOLTIP_MAX_WIDTH, TOOLTIP_HEIGHT_COLLAPSED);
        assert_eq!(anchor.x, button.center().x - TOOLTIP_MAX_WIDTH * 0.5);
        assert_eq!(anchor.y, button.top() - TOOLTIP_HEIGHT_COLLAPSED - 6.0);
        assert!(anchor.y + TOOLTIP_HEIGHT_COLLAPSED <= button.top());
    }

    #[test]
    fn bottom_hud_tooltip_anchor_clamps_to_the_left_window_edge() {
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let button = egui::Rect::from_min_size(pos2(2.0, 760.0), vec2(30.0, 30.0));
        let anchor =
            bottom_hud_tooltip_anchor(button, screen, TOOLTIP_MAX_WIDTH, TOOLTIP_HEIGHT_COLLAPSED);
        assert_eq!(anchor.x, 6.0);
    }

    #[test]
    fn bottom_hud_hover_sets_the_native_tooltip() {
        let mut state = ChromeState::default();
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let rail = crate::config::rail_width(state.rail_expanded);
        // Center of the first icon button in the bottom HUD strip.
        let hud_x = rail + crate::config::PAGE_HUD_MARGIN + 8.0 + super::PAGE_HUD_BUTTON_SIZE / 2.0;
        let hud_y = 800.0 - crate::config::PAGE_HUD_HEIGHT - crate::config::PAGE_HUD_MARGIN
            + super::PAGE_HUD_VERTICAL_MARGIN as f32
            + super::PAGE_HUD_BUTTON_SIZE / 2.0;
        let pointer = pos2(hud_x, hud_y);
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(screen);
        raw.events.push(egui::Event::PointerMoved(pointer));
        let mut actions = Vec::new();
        clear_textures(&ctx, raw, |ui| {
            ChromeRenderer::build_page_hud(ui.ctx(), &mut state, &mut actions);
        });
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(screen);
        raw.events.push(egui::Event::PointerMoved(pointer));
        clear_textures(&ctx, raw, |ui| {
            ChromeRenderer::build_page_hud(ui.ctx(), &mut state, &mut actions);
        });
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(screen);
        raw.events.push(egui::Event::PointerMoved(pointer));
        clear_textures(&ctx, raw, |ui| {
            ChromeRenderer::build_page_hud(ui.ctx(), &mut state, &mut actions);
        });
        assert!(state.tooltip.is_some());
        assert_eq!(
            state.tooltip.as_ref().map(|tooltip| tooltip.text.as_str()),
            Some("Command palette (Cmd/Ctrl+K)")
        );
    }

    #[test]
    fn page_tooltip_anchor_follows_row_and_stays_over_the_page() {
        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(RAIL_WIDTH_EXPANDED, 800.0));
        let row = egui::Rect::from_min_size(pos2(6.0, 200.0), vec2(168.0, 36.0));
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let anchor = page_tooltip_anchor(
            rail,
            row,
            screen,
            TOOLTIP_MAX_WIDTH,
            TOOLTIP_HEIGHT_EXPANDED,
        );
        assert_eq!(
            anchor,
            pos2(
                RAIL_WIDTH_EXPANDED + 8.0,
                row.center().y - TOOLTIP_HEIGHT_EXPANDED * 0.5,
            )
        );
        assert_eq!(anchor.y + TOOLTIP_HEIGHT_EXPANDED * 0.5, row.center().y);
        assert!(anchor.x >= rail.right());
        assert!(anchor.x + TOOLTIP_MAX_WIDTH <= screen.right());
        assert!(anchor.y + TOOLTIP_HEIGHT_EXPANDED <= screen.bottom());
    }

    #[test]
    fn page_tooltip_anchor_clamps_near_screen_bottom() {
        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(RAIL_WIDTH_EXPANDED, 800.0));
        let row = egui::Rect::from_min_size(pos2(6.0, 790.0), vec2(168.0, 36.0));
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let anchor = page_tooltip_anchor(
            rail,
            row,
            screen,
            TOOLTIP_MAX_WIDTH,
            TOOLTIP_HEIGHT_EXPANDED,
        );
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
        assert_eq!(items[5].action, CommandAction::OpenBrowserImport);
        assert_eq!(items[6].action, CommandAction::OpenDownloads);
        assert_eq!(items[7].action, CommandAction::OpenHistory);
        assert_eq!(items[8].action, CommandAction::Print);
        assert_eq!(items[9].action, CommandAction::SelectTab(1));
        assert_eq!(items[10].action, CommandAction::BookmarkActiveTab);
        assert_eq!(items[11].action, CommandAction::OpenBookmarkManager);
        assert_eq!(items[12].action, CommandAction::NewPrivateWindow);
    }

    #[test]
    fn palette_finds_incognito_action_and_marks_private_tabs() {
        let prefs = BrowserPreferences::default();
        let items = command_palette_items("private", &[], None, &prefs);
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::NewPrivateWindow));

        let mut private = tab(4, "Secret", "https://secret.example");
        private.workspace_id = Workspace::PRIVATE_ID;
        private.workspace_name = Workspace::PRIVATE_NAME.to_string();
        let items = command_palette_items("secret", &[private], None, &prefs);
        assert!(items.iter().any(|item| {
            item.icon == String::from(ICON_VISIBILITY_OFF)
                && item.action == CommandAction::SelectTab(4)
        }));
    }

    #[test]
    fn palette_finds_the_downloads_bubble() {
        let items = command_palette_items("download", &[], None, &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::OpenDownloads));
    }

    #[test]
    fn palette_icons_are_registered_material_glyphs() {
        let prefs = BrowserPreferences::default();
        let mut private = tab(9, "Secret", "https://secret.example");
        private.workspace_id = Workspace::PRIVATE_ID;
        let tabs = vec![tab(1, "Docs", "https://docs.example"), private];

        for query in [
            "", "private", "bookmark", "download", "export", "history", "settings",
        ] {
            let items = command_palette_items(query, &tabs, Some(1), &prefs);
            assert!(!items.is_empty(), "no palette rows for {query:?}");
            for item in items {
                let is_material_glyph = item.icon.chars().count() == 1
                    && item.icon.chars().all(|glyph| {
                        matches!(
                            glyph,
                            '\u{E000}'..='\u{F8FF}'
                                | '\u{F0000}'..='\u{FFFFD}'
                                | '\u{100000}'..='\u{10FFFD}'
                        )
                    });
                assert!(
                    is_material_glyph,
                    "palette icon {:?} for {:?} is not a Material Symbols glyph",
                    item.icon, item.label
                );
            }
        }
    }

    #[test]
    fn palette_finds_the_history_manager() {
        let items = command_palette_items("history", &[], None, &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::OpenHistory));

        let mut state = ChromeState::default();
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::OpenHistory, &mut state, &mut actions);
        assert_eq!(actions, vec![UiAction::OpenHistory]);
    }

    #[test]
    fn palette_finds_portable_export_actions() {
        let items = command_palette_items("export", &[], None, &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::ExportBookmarks));
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::ExportHistory));

        let mut state = ChromeState::default();
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::ExportBookmarks, &mut state, &mut actions);
        assert_eq!(
            actions,
            vec![UiAction::ExportBrowserData {
                bookmarks: true,
                history: false,
            }]
        );

        actions.clear();
        apply_command_item(&CommandAction::ExportHistory, &mut state, &mut actions);
        assert_eq!(
            actions,
            vec![UiAction::ExportBrowserData {
                bookmarks: false,
                history: true,
            }]
        );
    }

    #[test]
    fn history_manager_filters_and_formats_entries() {
        let items = vec![
            HistoryItem {
                url: "https://docs.example/rust".to_string(),
                title: "Rust docs".to_string(),
                visit_count: 2,
                last_visited: 3_600,
            },
            HistoryItem {
                url: "https://news.example".to_string(),
                title: "News".to_string(),
                visit_count: 1,
                last_visited: 100_000,
            },
        ];

        assert_eq!(filter_history(&items, "").len(), 2);
        assert_eq!(filter_history(&items, "RUST").len(), 1);
        assert_eq!(filter_history(&items, "news.example").len(), 1);
        assert!(filter_history(&items, "missing").is_empty());
        assert_eq!(filter_history_indices(&items, "RUST"), vec![0]);
        assert_eq!(filter_history_indices(&items, ""), vec![0, 1]);

        let mut state = ChromeState::default();
        assert_eq!(state.history_visible_count, HISTORY_VISIBLE_PAGE_SIZE);
        state.open_history();
        state.show_more_history();
        assert_eq!(state.history_visible_count, HISTORY_VISIBLE_PAGE_SIZE * 2);
        state.close_history();
        assert_eq!(state.history_visible_count, HISTORY_VISIBLE_PAGE_SIZE);

        assert_eq!(format_history_time(100_000, 100_000), "just now");
        assert_eq!(format_history_time(100_000, 100_050), "just now");
        assert_eq!(format_history_time(100_000, 100_100), "1m ago");
        assert_eq!(format_history_time(90, 3_600), "58m ago");
        assert_eq!(format_history_time(0, 7_200), "2h ago");
        assert_eq!(format_history_time(0, 100_000), "yesterday");
        assert_eq!(format_history_time(0, 500_000), "5d ago");
    }

    #[test]
    fn history_overlay_is_modal_and_exclusive() {
        let mut state = ChromeState::default();
        assert!(!state.has_modal_overlay());

        state.open_history();
        assert!(state.history_open);
        assert!(state.has_modal_overlay());

        state.open_bookmark_manager();
        assert!(!state.history_open);
        assert!(state.bookmark_manager_open);

        state.open_history();
        state.set_history(Vec::new());
        state.close_history();
        assert!(!state.history_open);
        assert!(!state.has_modal_overlay());
    }

    #[test]
    fn command_palette_bookmarks_the_active_tab_and_confirms() {
        let items = command_palette_items(
            "bookmark",
            &[tab(9, "Docs", "https://docs.example")],
            Some(9),
            &BrowserPreferences::default(),
        );
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::BookmarkActiveTab));

        let mut state = ChromeState::default();
        state.active_tab_id = Some(9);
        state.command_palette_open = true;
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::BookmarkActiveTab, &mut state, &mut actions);
        assert_eq!(actions, vec![UiAction::AddBookmark { tab_id: 9 }]);
        assert!(!state.command_palette_open);
        assert_eq!(state.pending_toasts.len(), 1);
        state.promote_pending_toast();
        let toast = state.toast.expect("bookmark confirmation toast");
        assert_eq!(toast.kind, ToastKind::Success);
        assert_eq!(toast.title, "Bookmark saved");
    }

    #[test]
    fn active_bookmark_key_matches_only_the_active_workspace_url() {
        let mut state = ChromeState::default();
        state.tabs = vec![tab(9, "Docs", "https://docs.example")];
        state.active_tab_id = Some(9);
        assert_eq!(active_bookmark_key(&state), None);

        state.bookmarks = vec![bookmark(-1, "Docs", "https://docs.example", "Personal")];
        assert_eq!(
            active_bookmark_key(&state),
            Some((1, "https://docs.example".to_string()))
        );

        state.bookmarks[0].workspace_id = 2;
        assert_eq!(active_bookmark_key(&state), None);
    }

    #[test]
    fn bookmark_star_outline_is_a_closed_ten_point_path() {
        let rect = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(12.0, 12.0));
        let points = bookmark_star_points(rect);
        assert_eq!(points.len(), 11);
        assert_eq!(points.first(), points.last());
        assert!(points.iter().any(|point| point.y < 1.0));
        assert!(points.iter().any(|point| (1.0..5.0).contains(&point.y)));
    }

    #[test]
    fn command_palette_removes_bookmark_and_confirms() {
        let mut state = ChromeState::default();
        state.tabs = vec![tab(9, "Docs", "https://docs.example")];
        state.active_tab_id = Some(9);
        state.bookmarks = vec![bookmark(-1, "Docs", "https://docs.example", "Personal")];
        state.command_palette_open = true;
        let mut actions = Vec::new();

        apply_command_item(&CommandAction::BookmarkActiveTab, &mut state, &mut actions);

        assert_eq!(
            actions,
            vec![UiAction::DeleteBookmark {
                workspace_id: 1,
                url: "https://docs.example".to_string(),
            }]
        );
        assert!(!state.command_palette_open);
        assert_eq!(state.pending_toasts.len(), 1);
        state.promote_pending_toast();
        let toast = state.toast.expect("bookmark removal toast");
        assert_eq!(toast.kind, ToastKind::Success);
        assert_eq!(toast.title, "Bookmark removed");
    }

    #[test]
    fn palette_finds_the_bookmark_manager() {
        let items = command_palette_items("bookmark", &[], None, &BrowserPreferences::default());
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::OpenBookmarkManager));
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
    fn palette_offers_print_action() {
        let items = command_palette_items("print", &[], Some(7), &BrowserPreferences::default());
        assert!(items.iter().any(|item| item.action == CommandAction::Print));

        let mut state = ChromeState::default();
        state.active_tab_id = Some(7);
        state.command_palette_open = true;
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::Print, &mut state, &mut actions);
        assert_eq!(actions, vec![UiAction::Print { tab_id: 7 }]);
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
    fn find_bar_geometry_stays_centered_and_inside_the_viewport() {
        let normal = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1440.0, 900.0));
        let (width, x, y) = find_bar_geometry(normal);
        assert_eq!(width, 640.0);
        assert_eq!(x, 400.0);
        assert_eq!(y, 14.0);

        let secondary_display = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(2880.0, 1800.0));
        let (width, x, y) = find_bar_geometry(secondary_display);
        assert_eq!(width, 640.0);
        assert_eq!(x, 1120.0);
        assert_eq!(y, 14.0);

        let tiny = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 60.0));
        let (width, x, y) = find_bar_geometry(tiny);
        assert_eq!(width, 184.0);
        assert_eq!(x, 8.0);
        assert_eq!(y, 4.0);
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
            clear_textures(
                &ctx,
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        pos2(0.0, 0.0),
                        vec2(1280.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| ChromeRenderer::build_downloads_bubble(ui.ctx(), &mut state, &mut actions),
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
    fn bookmark_manager_renders_search_and_editor_without_invalid_layout() {
        let ctx = egui::Context::default();
        let mut state = ChromeState::default();
        state.bookmark_manager_open = true;
        state.workspaces = vec![WorkspaceInfo {
            id: 1,
            name: "Personal".to_string(),
            icon: WorkspaceIcon::Work,
            color: WorkspaceColor::Blue,
            is_default: true,
            is_active: true,
        }];
        state.bookmarks = vec![bookmark(
            1,
            "Rust docs",
            "https://doc.rust-lang.org",
            "Personal",
        )];
        state.bookmark_search_query = "rust".to_string();
        state.selected_bookmark_key = Some((1, "https://doc.rust-lang.org".to_string()));
        state.bookmark_title_draft = "Rust docs".to_string();
        state.bookmark_url_draft = "https://doc.rust-lang.org".to_string();
        state.bookmark_workspace_id_draft = 1;
        state.bookmark_folder_draft = "Personal".to_string();
        let mut actions = Vec::new();

        for _ in 0..3 {
            clear_textures(
                &ctx,
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        pos2(0.0, 0.0),
                        vec2(1280.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| ChromeRenderer::build_bookmark_manager(ui.ctx(), &mut state, &mut actions),
            );
            if let Some(area) =
                egui::AreaState::load(&ctx, egui::Id::new("kasumi_bookmark_manager"))
            {
                let size = area.size.unwrap_or_default();
                assert!(
                    size.x.is_finite() && size.y.is_finite(),
                    "Bookmark manager area size must stay finite: {size:?}"
                );
            }
        }

        assert!(actions.is_empty());
        assert!(state.bookmark_manager_open);
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

        let expanded = downloads_bubble_anchor(screen, RAIL_WIDTH_EXPANDED);
        assert!(expanded.x >= RAIL_WIDTH_EXPANDED);
        assert!(expanded.y >= screen.top());
        assert!(expanded.y + size.y <= screen.bottom());

        let collapsed = downloads_bubble_anchor(screen, 44.0);
        assert!(collapsed.x < expanded.x);
        assert!(collapsed.x >= 44.0);

        let small = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(220.0, 180.0));
        let small_size = downloads_bubble_size(small);
        let anchor = downloads_bubble_anchor(small, RAIL_WIDTH_EXPANDED);
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
    fn command_palette_open_import_action_opens_import_settings() {
        let mut state = ChromeState::default();
        let mut actions = Vec::new();
        apply_command_item(&CommandAction::OpenBrowserImport, &mut state, &mut actions);

        assert!(state.settings_open);
        assert_eq!(state.settings_tab, SettingsTab::Import);
        assert!(actions.is_empty());
    }

    #[test]
    fn browser_import_state_tracks_profiles_progress_and_report() {
        let mut state = ChromeState::default();
        let profile = BrowserProfile {
            source: BrowserSource::Chrome,
            bookmarks_path: Some(PathBuf::from("/profiles/chrome/Bookmarks")),
            history_path: Some(PathBuf::from("/profiles/chrome/History")),
        };
        state.browser_import_detecting = true;
        state.set_browser_profiles(vec![profile.clone()]);
        assert!(!state.browser_import_detecting);
        assert_eq!(state.browser_import_profiles, vec![profile]);

        state.set_browser_import_progress("Reading bookmarks".to_string());
        assert_eq!(state.browser_import_progress, "Reading bookmarks");

        state.browser_import_running = true;
        state.set_browser_import_report(ImportReport {
            bookmarks_added: 3,
            history_added: 7,
            ..ImportReport::default()
        });
        assert!(!state.browser_import_running);
        assert!(state.browser_import_progress.is_empty());
        assert_eq!(
            state
                .browser_import_report
                .as_ref()
                .map(|r| r.bookmarks_added),
            Some(3)
        );
    }

    #[test]
    fn tab_menu_metrics_and_anchor_stay_beside_rail() {
        let mut state = ChromeState::default();
        state.tabs = vec![tab(7, "Seven", "https://seven.example")];
        let (width, height) = rail_menu_metrics(&state, 7);
        assert_eq!(width, 224.0);
        assert_eq!(
            height,
            rail_menu_items(&state, 7).len() as f32 * 28.0 + 14.0
        );

        let rail = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(RAIL_WIDTH_EXPANDED, 800.0));
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
        // Ten base commands, the move heading, and the empty-target hint.
        assert_eq!(items.len(), 12);
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
                .map(|item| item.label.as_str()),
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
                .map(|item| item.label.as_str()),
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
    fn close_other_tabs_is_disabled_when_only_other_tabs_are_pinned() {
        let mut state = ChromeState::default();
        let mut pinned = tab(2, "Pinned", "https://pinned.example");
        pinned.is_pinned = true;
        state.tabs = vec![tab(1, "Current", "https://current.example"), pinned];

        let items = rail_menu_items(&state, 1);
        assert!(!items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseOtherTabs && item.enabled));

        // Pinned tabs are never in the close set, but a pinned source tab can
        // still close other unpinned tabs.
        let items = rail_menu_items(&state, 2);
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::CloseOtherTabs && item.enabled));
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
                .map(|item| item.label.as_str()),
            Some("Unpin tab")
        );
    }

    #[test]
    fn tab_menu_moves_unpinned_tabs_between_workspaces() {
        let mut state = ChromeState::default();
        state.workspaces = vec![
            WorkspaceInfo {
                id: 1,
                name: "Personal".to_string(),
                icon: WorkspaceIcon::Work,
                color: WorkspaceColor::Blue,
                is_default: true,
                is_active: true,
            },
            WorkspaceInfo {
                id: 2,
                name: "Work".to_string(),
                icon: WorkspaceIcon::Code,
                color: WorkspaceColor::Green,
                is_default: false,
                is_active: false,
            },
        ];
        state.tabs = vec![tab(7, "Seven", "https://seven.example")];

        let items = rail_menu_items(&state, 7);
        assert!(items.iter().any(|item| {
            item.action == RailMenuAction::MoveTabToWorkspace { workspace_id: 2 } && item.enabled
        }));
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::MoveTabToWorkspace { workspace_id: 2 }, 7),
            Some(UiAction::MoveTabToWorkspace {
                tab_id: 7,
                workspace_id: 2,
            })
        );
    }

    #[test]
    fn tab_menu_does_not_offer_move_for_pinned_tabs() {
        let mut state = ChromeState::default();
        state.workspaces = vec![
            WorkspaceInfo {
                id: 1,
                name: "Personal".to_string(),
                icon: WorkspaceIcon::Work,
                color: WorkspaceColor::Blue,
                is_default: true,
                is_active: true,
            },
            WorkspaceInfo {
                id: 2,
                name: "Work".to_string(),
                icon: WorkspaceIcon::Code,
                color: WorkspaceColor::Green,
                is_default: false,
                is_active: false,
            },
        ];
        let mut pinned = tab(7, "Pinned", "https://pinned.example");
        pinned.is_pinned = true;
        state.tabs = vec![pinned];

        assert!(!rail_menu_items(&state, 7)
            .iter()
            .any(|item| { matches!(item.action, RailMenuAction::MoveTabToWorkspace { .. }) }));
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

    #[test]
    fn notification_icon_context_registers_material_symbol_font() {
        let ctx = egui::Context::default();
        register_material_icon_font(&ctx);
        // egui builds its font atlas on the first frame.
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();

        let families = ctx.fonts(|fonts| fonts.families());
        assert!(families.iter().any(|family| matches!(
            family,
            egui::FontFamily::Name(name) if name.as_ref() == egui_material_icons::FONT_FAMILY
        )));
        assert!(ctx.fonts_mut(|fonts| {
            fonts.has_glyph(
                &egui::FontId::new(16.0, egui::FontFamily::Proportional),
                '\u{e5cd}',
            )
        }));
    }
}
