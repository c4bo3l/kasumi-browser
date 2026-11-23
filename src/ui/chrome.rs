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
//! `x = rail_width` and fills the remaining window height. The command palette
//! and rail tooltips use transparent native overlay windows so they appear
//! above that child view; the browser keeps running underneath and the page
//! stays visible. This file never touches CEF directly.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{
    pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Frame, Layout, Order, Response,
    RichText, Rounding, Sense, Stroke, TextEdit, TextureOptions, Ui, UiBuilder,
};
use egui_wgpu::ScreenDescriptor;

use crate::config::{
    rail_width, COMMAND_PALETTE_TOP_OFFSET, COMMAND_PALETTE_WIDTH, DEFAULT_SEARCH_URL_PREFIX,
};
use crate::events::UiAction;

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
        tooltip_bg: Color32::from_rgb(16, 19, 26),
        tooltip_border: Color32::from_rgb(110, 130, 160),
        tooltip_text: Color32::from_rgb(250, 251, 255),
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
        tooltip_bg: Color32::from_rgb(255, 255, 255),
        tooltip_border: Color32::from_rgb(150, 166, 188),
        tooltip_text: Color32::from_rgb(25, 28, 33),
    };
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

const TOOLTIP_WIDTH: f32 = 240.0;
const TOOLTIP_HEIGHT_EXPANDED: f32 = 52.0;
const TOOLTIP_HEIGHT_COLLAPSED: f32 = 40.0;
const RAIL_MENU_WIDTH: f32 = 224.0;
const RAIL_MENU_ROW_HEIGHT: f32 = 28.0;
const RAIL_MENU_PADDING: f32 = 7.0;

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
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub display_state: TabDisplayState,
    pub blocked_requests: u32,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub favicon: Option<Favicon>,
}

/// An action produced by selecting a command-palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    SelectTab(u64),
    NewTab(Option<String>),
    Navigate { tab_id: u64, url: String },
    HibernateAll,
    ToggleRail,
    OpenDevTools,
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
    /// Set when the palette should focus and select its query on next render.
    pub command_palette_focus_requested: bool,
    /// Latest rail tooltip; consumed by the overlay window on each frame.
    pub tooltip: Option<RailTooltip>,
    /// Open tab context menu; consumed by the shared full-window overlay.
    pub menu: Option<RailMenu>,
    /// Currently highlighted context-menu row.
    pub menu_selection: usize,
    /// True when at least one closed tab can be restored with Cmd/Ctrl+Z.
    pub can_reopen_tab: bool,
    /// Current OS light/dark appearance used by every native chrome surface.
    pub theme: ChromeTheme,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab_id: None,
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
            tooltip: None,
            menu: None,
            menu_selection: 0,
            can_reopen_tab: false,
            theme: ChromeTheme::default(),
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
        }
        self.command_palette_open = true;
        self.close_tab_menu();
    }

    pub fn close_command_palette(&mut self) {
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;
        self.tooltip = None;
        self.close_tab_menu();
    }

    pub fn toggle_rail(&mut self) {
        self.rail_expanded = !self.rail_expanded;
        self.close_tab_menu();
    }

    pub fn open_tab_menu(&mut self, tab_id: u64, anchor: egui::Pos2) {
        // The palette and menu share one transparent overlay window and must
        // never both claim it. Clearing the palette here also avoids a stale
        // focus request when the menu is dismissed.
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_selection = 0;
        self.command_palette_focus_requested = false;

        let (width, height) = rail_menu_metrics();
        self.menu = Some(RailMenu {
            tab_id,
            anchor,
            width,
            height,
        });
        self.menu_selection = 0;
        self.tooltip = None;
    }

    pub fn close_tab_menu(&mut self) {
        self.menu = None;
        self.menu_selection = 0;
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
        state.theme.apply_visuals(&self.egui_ctx);

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
        let palette = state.theme.palette();
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

                        let expand = Self::rail_icon_button(
                            ui,
                            vec2(34.0, 30.0),
                            RailIcon::ChevronRight,
                            palette,
                        );
                        if expand.clicked() {
                            state.toggle_rail();
                        }
                        Self::chrome_tooltip(
                            tooltip_slot,
                            ui,
                            &expand,
                            "Expand workspace rail",
                            state.rail_expanded,
                        );

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
                            "DevTools (F12, new window)",
                            state.rail_expanded,
                        );

                        ui.add_space(5.0);
                        ui.separator();
                        ui.add_space(5.0);

                        egui::ScrollArea::vertical()
                            .id_salt("favicon_rail_tabs")
                            .show(ui, |ui| {
                                for tab in &state.tabs {
                                    let tab_id = tab.id;
                                    let is_active = state.active_tab_id == Some(tab_id);
                                    let (row_rect, row_resp) = ui.allocate_exact_size(
                                        vec2(width - 6.0, 44.0),
                                        Sense::click(),
                                    );

                                    Self::paint_tab_row(
                                        ui,
                                        row_rect,
                                        is_active,
                                        row_resp.hovered(),
                                        palette,
                                    );
                                    let icon_rect = egui::Rect::from_center_size(
                                        row_rect.center(),
                                        vec2(24.0, 24.0),
                                    );
                                    self.paint_favicon_in_rect(ui, tab, icon_rect, palette);
                                    let close_resp = Self::paint_tab_close(
                                        ui,
                                        row_rect,
                                        row_resp.hovered(),
                                        tab_id,
                                        false,
                                        palette,
                                    );
                                    Self::paint_tab_status(ui, row_rect, tab, false, palette);
                                    Self::chrome_tooltip(
                                        tooltip_slot,
                                        ui,
                                        &close_resp,
                                        format!("Close tab: {}", tab.title),
                                        state.rail_expanded,
                                    );

                                    if close_resp.clicked() {
                                        actions.push(UiAction::CloseTab { tab_id });
                                    } else if row_resp.clicked() {
                                        actions.push(UiAction::SelectTab { tab_id });
                                    }
                                    if row_resp.middle_clicked() {
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
                                        menu_request = Some((tab_id, anchor));
                                    }
                                    let tooltip_text = if state.rail_expanded {
                                        format!("{}\n{}", tab.title, tab.url)
                                    } else {
                                        tab.title.clone()
                                    };
                                    Self::chrome_tooltip(
                                        tooltip_slot,
                                        ui,
                                        &row_resp,
                                        tooltip_text,
                                        state.rail_expanded,
                                    );
                                }
                            });
                    });
                    return;
                }

                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    let rail_rect = ui.clip_rect();
                    ui.add_space(7.0);
                    ui.add_space(3.0);
                    let collapse = Self::rail_action_button(
                        ui,
                        width - 12.0,
                        30.0,
                        RailIcon::ChevronLeft,
                        "Collapse rail",
                        palette,
                    );
                    if collapse.clicked() {
                        state.toggle_rail();
                    }
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("KASUMI")
                                .size(12.0)
                                .strong()
                                .color(palette.brand),
                        );
                    });

                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);

                    ui.add_space(2.0);
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
                        "Open command palette (Cmd/Ctrl+K)",
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
                        "DevTools (F12, new window)",
                        state.rail_expanded,
                    );

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("WORKSPACE")
                                .size(9.0)
                                .color(palette.section_label),
                        );
                    });
                    ui.add_space(2.0);

                    egui::ScrollArea::vertical()
                        .id_salt("rail_tabs")
                        .show(ui, |ui| {
                            for tab in &state.tabs {
                                let tab_id = tab.id;
                                let is_active = state.active_tab_id == Some(tab_id);
                                let title = if tab.title.is_empty() {
                                    "New Tab".to_string()
                                } else {
                                    truncate_title(&tab.title, 18)
                                };

                                let (row_rect, row_resp) = ui
                                    .allocate_exact_size(vec2(width - 12.0, 36.0), Sense::click());

                                // Paint the background and its contents against the
                                // same rect. Previously the labels were added after
                                // the row had advanced the cursor, so they appeared
                                // below the highlight instead of inside it.
                                Self::paint_tab_row(
                                    ui,
                                    row_rect,
                                    is_active,
                                    row_resp.hovered(),
                                    palette,
                                );
                                let icon_rect = egui::Rect::from_min_size(
                                    pos2(row_rect.min.x + 8.0, row_rect.center().y - 10.0),
                                    vec2(20.0, 20.0),
                                );
                                self.paint_favicon_in_rect(ui, tab, icon_rect, palette);

                                let text_color = if is_active {
                                    palette.text_strong
                                } else {
                                    palette.text
                                };
                                ui.painter().text(
                                    pos2(row_rect.min.x + 34.0, row_rect.center().y),
                                    Align2::LEFT_CENTER,
                                    title,
                                    FontId::new(12.0, FontFamily::Proportional),
                                    text_color,
                                );
                                let close_resp = Self::paint_tab_close(
                                    ui,
                                    row_rect,
                                    row_resp.hovered(),
                                    tab_id,
                                    true,
                                    palette,
                                );
                                Self::paint_tab_status(
                                    ui,
                                    row_rect,
                                    tab,
                                    row_resp.hovered() || close_resp.hovered(),
                                    palette,
                                );

                                if close_resp.clicked() {
                                    actions.push(UiAction::CloseTab { tab_id });
                                } else if row_resp.clicked() {
                                    actions.push(UiAction::SelectTab { tab_id });
                                }
                                if row_resp.middle_clicked() {
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
                                    menu_request = Some((tab_id, anchor));
                                }
                                let tooltip_text = if state.rail_expanded {
                                    format!("{}\n{}", tab.title, tab.url)
                                } else {
                                    tab.title.clone()
                                };
                                Self::chrome_tooltip(
                                    tooltip_slot,
                                    ui,
                                    &row_resp,
                                    tooltip_text,
                                    state.rail_expanded,
                                );
                            }
                        });
                });

                // Status block stays at the bottom of the rail.
                let status_rect = egui::Rect::from_min_max(
                    pos2(0.0, ui.clip_rect().bottom() - 78.0),
                    pos2(width, ui.clip_rect().bottom()),
                );
                ui.allocate_new_ui(UiBuilder::new().max_rect(status_rect), |ui| {
                    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                        ui.add_space(6.0);
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
                            "Hibernate all background tabs (Alt+H)",
                            state.rail_expanded,
                        );

                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let (icon_rect, _) =
                                ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
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
                            let (icon_rect, _) =
                                ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
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
        let palette = state.theme.palette();

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
        let palette = state.theme.palette();

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

    /// Loading/hibernated marker anchored inside the tab row. It shifts left
    /// while the close button is visible so the two never overlap.
    fn paint_tab_status(
        ui: &Ui,
        row_rect: egui::Rect,
        tab: &TabInfo,
        close_visible: bool,
        palette: &ChromePalette,
    ) {
        let center = tab_status_center(row_rect, close_visible);
        if tab.is_loading {
            ui.painter().circle_filled(center, 3.5, palette.loading);
        } else if tab.display_state == TabDisplayState::Hibernated {
            paint_rail_icon(
                ui,
                egui::Rect::from_center_size(center, vec2(12.0, 12.0)),
                RailIcon::Moon,
                palette.text_dim,
            );
        }
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
        let menu_open = state.menu.is_some();
        let focus_before = state.command_palette_focus_requested && !menu_open;
        state.theme.apply_visuals(&self.egui_ctx);

        if focus_before {
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
            if menu_open {
                ChromeRenderer::build_tab_menu(ctx, state, &mut actions);
            } else {
                ChromeRenderer::build_command_palette(ctx, state, &mut actions, focus_command);
            }
        });
        // The rail button raises this flag during the main window's render,
        // which happens before this overlay's first frame. Consume it here.
        state.command_palette_focus_requested =
            focus_request_after_render(focus_before, state.command_palette_focus_requested);

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
        theme: ChromeTheme,
    ) {
        self.resize(window.inner_size());
        theme.apply_visuals(&self.egui_ctx);
        let palette = theme.palette();
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
                        .rounding(Rounding::same(7.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 7.0))
                        .show(ui, |ui| {
                            // Fill the whole overlay window so the visible card is
                            // vertically centered on the hovered tab row. A plain
                            // label sizes the frame to its text and leaves the card
                            // pinned near the top of the taller transparent window.
                            let inner_size = vec2(
                                (card.width - 16.0).max(80.0),
                                (card.height - 14.0).max(12.0),
                            );
                            let (rect, _) = ui.allocate_exact_size(inner_size, Sense::hover());
                            let lines: Vec<&str> = card.text.lines().collect();
                            let line_height = 14.0_f32;
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
fn paint_rail_icon(ui: &Ui, rect: egui::Rect, icon: RailIcon, color: Color32) {
    use egui_material_icons::icons::{
        ICON_ADD, ICON_BEDTIME, ICON_CHEVRON_LEFT, ICON_CHEVRON_RIGHT, ICON_CLOSE, ICON_CODE,
        ICON_MEMORY, ICON_PUBLIC, ICON_SEARCH, ICON_SHIELD,
    };

    let glyph = match icon {
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
    };

    let size = rect.width().min(rect.height()) * 1.08;
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::new(size, FontFamily::Proportional),
        color,
    );
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

/// Fixed card size for the rail tab context menu.
pub fn rail_menu_metrics() -> (f32, f32) {
    const ITEM_COUNT: usize = 9;
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
    let index = state.tabs.iter().position(|tab| tab.id == tab_id);
    let tab_count = state.tabs.len();
    let item = |label, action, enabled| RailMenuItem {
        label,
        action,
        enabled,
    };
    vec![
        item("New tab", RailMenuAction::NewTab, true),
        item("Duplicate tab", RailMenuAction::DuplicateTab, true),
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
) -> Vec<CommandPaletteItem> {
    let trimmed = query.trim();
    let mut items = Vec::new();

    let tab_item = |tab: &TabInfo| CommandPaletteItem {
        icon: if tab.display_state == TabDisplayState::Hibernated {
            "💤"
        } else {
            "◧"
        },
        label: if tab.title.is_empty() {
            "New Tab".to_string()
        } else {
            tab.title.clone()
        },
        subtitle: tab.url.clone(),
        action: CommandAction::SelectTab(tab.id),
    };

    if trimmed.is_empty() {
        items.push(CommandPaletteItem {
            icon: "＋",
            label: "New tab".to_string(),
            subtitle: "Open a fresh tab".to_string(),
            action: CommandAction::NewTab(None),
        });
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
        items.extend(tabs.iter().take(12).map(tab_item));
        return items;
    }

    let needle = trimmed.to_lowercase();
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

    let url = resolve_omnibox(trimmed);
    let looks_like_address = trimmed.contains('.')
        || trimmed.contains("://")
        || trimmed.starts_with("view-source:")
        || trimmed.starts_with("data:");

    let label = if looks_like_address {
        format!("Go to {trimmed}")
    } else {
        format!("Search the web for “{trimmed}”")
    };

    if let Some(tab_id) = active_tab_id {
        items.push(CommandPaletteItem {
            icon: "↗",
            label,
            subtitle: url.clone(),
            action: CommandAction::Navigate {
                tab_id,
                url: url.clone(),
            },
        });
    }
    items.push(CommandPaletteItem {
        icon: "＋",
        label: "Open in a new tab".to_string(),
        subtitle: url,
        action: CommandAction::NewTab(Some(resolve_omnibox(trimmed))),
    });

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
        CommandAction::HibernateAll => actions.push(UiAction::HibernateAllBackground),
        CommandAction::ToggleRail => state.toggle_rail(),
        CommandAction::OpenDevTools => {
            if let Some(tab_id) = state.active_tab_id {
                actions.push(UiAction::OpenDevTools { tab_id });
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
// Omnibox resolution logic
// ---------------------------------------------------------------------------

/// Convert omnibox input to a navigable URL per AGENTS.md spec.
pub fn resolve_omnibox(input: &str) -> String {
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
    format!("{}{}", DEFAULT_SEARCH_URL_PREFIX, encoded)
}

#[cfg(test)]
mod tests {
    use egui::{pos2, vec2, Color32};

    use super::{
        advance_command_selection, apply_command_item, clamp_command_selection,
        command_palette_click_outside, command_palette_items, focus_request_after_render,
        page_tooltip_anchor, palette_row_fill, preferred_surface_format, rail_menu_anchor,
        rail_menu_click_outside, rail_menu_items, rail_menu_metrics, rail_menu_ui_action,
        rail_tooltip_enabled, rail_tooltip_metrics, resolve_omnibox, tab_close_center,
        tab_status_center, truncate_title, ChromePalette, ChromeState, ChromeTheme, CommandAction,
        RailMenuAction, TabDisplayState, TabInfo, TOOLTIP_HEIGHT_COLLAPSED,
        TOOLTIP_HEIGHT_EXPANDED, TOOLTIP_WIDTH,
    };
    use crate::config::{DEFAULT_HOME_URL, DEFAULT_SEARCH_URL_PREFIX};
    use crate::events::UiAction;

    fn tab(id: u64, title: &str, url: &str) -> TabInfo {
        TabInfo {
            id,
            url: url.to_string(),
            title: title.to_string(),
            is_loading: false,
            display_state: TabDisplayState::Background,
            blocked_requests: 0,
            can_go_back: false,
            can_go_forward: false,
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
    fn collapsed_tooltips_are_readable_and_render_over_the_page() {
        let (width, height) = rail_tooltip_metrics(false);
        assert_eq!(width, TOOLTIP_WIDTH);
        assert_eq!(height, TOOLTIP_HEIGHT_COLLAPSED);
        assert!(width >= 200.0);

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
        assert_eq!(anchor, pos2(188.0, 192.0));
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
        assert_eq!(anchor.y, 742.0);
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
        let items = command_palette_items("", &[tab(1, "Rust", "https://rust-lang.org")], Some(1));
        assert_eq!(items[0].action, CommandAction::NewTab(None));
        assert_eq!(items[1].action, CommandAction::HibernateAll);
        assert_eq!(items[2].action, CommandAction::ToggleRail);
        assert_eq!(items[3].action, CommandAction::SelectTab(1));
    }

    #[test]
    fn palette_filters_tabs_case_insensitively() {
        let tabs = [
            tab(1, "Rust Language", "https://rust-lang.org"),
            tab(2, "Docs", "https://docs.rs"),
        ];
        let items = command_palette_items("RUST", &tabs, Some(1));
        assert!(items
            .iter()
            .any(|item| item.action == CommandAction::SelectTab(1)));
        assert!(!items
            .iter()
            .any(|item| item.action == CommandAction::SelectTab(2)));
    }

    #[test]
    fn palette_offers_navigation_and_new_tab_for_queries() {
        let items = command_palette_items("hello world", &[], Some(7));
        assert_eq!(
            items[0].action,
            CommandAction::Navigate {
                tab_id: 7,
                url: format!("{DEFAULT_SEARCH_URL_PREFIX}hello+world"),
            }
        );
        assert_eq!(
            items[1].action,
            CommandAction::NewTab(Some(format!("{DEFAULT_SEARCH_URL_PREFIX}hello+world")))
        );
    }

    #[test]
    fn palette_offers_devtools_action() {
        let items = command_palette_items("devtools", &[], Some(7));
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
    fn state_open_palette_seeds_query_and_close_resets_it() {
        let mut state = ChromeState::default();
        state.omnibox_text = "https://example.com".to_string();
        state.open_command_palette();
        assert!(state.command_palette_open);
        assert_eq!(state.command_palette_query, "https://example.com");
        assert!(state.command_palette_focus_requested);

        state.close_command_palette();
        assert!(!state.command_palette_open);
        assert!(state.command_palette_query.is_empty());
        assert!(!state.command_palette_focus_requested);
    }

    #[test]
    fn tab_menu_metrics_and_anchor_stay_beside_rail() {
        let (width, height) = rail_menu_metrics();
        assert_eq!(width, 224.0);
        assert_eq!(height, 9.0 * 28.0 + 14.0);

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
        let card = egui::Rect::from_min_size(pos2(188.0, 92.0), vec2(224.0, 238.0));
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
        assert_eq!(items.len(), 9);
        assert!(items
            .iter()
            .any(|item| item.action == RailMenuAction::NewTab && item.enabled));
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
    fn tab_menu_actions_map_to_tab_manager_commands() {
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::NewTab, 3),
            Some(UiAction::NewTab { url: None })
        );
        assert_eq!(
            rail_menu_ui_action(RailMenuAction::DuplicateTab, 3),
            Some(UiAction::DuplicateTab { tab_id: 3 })
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
