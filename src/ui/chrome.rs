//! egui + wgpu browser chrome: tab strip (38 px) + toolbar (38 px) = 76 px total.
//!
//! # Architecture
//! `ChromeRenderer` owns the wgpu surface, device/queue, and egui context.  
//! Each frame the caller:
//! 1. Updates `ChromeState` with the current tab list / active-tab info.
//! 2. Calls `render()` → draws the chrome and returns any `UiAction`s the user triggered.
//!
//! The CEF content renders entirely in a separate native OS child window that sits
//! *below* the chrome at y-offset 76 px (logical).  This file never touches CEF.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{
    pos2, vec2, Align, Color32, Frame, Layout, Response, RichText, Rounding, Sense, Stroke,
    TextEdit, TextureOptions, Ui,
};
use egui_wgpu::ScreenDescriptor;

use crate::config::{CHROME_HEIGHT, DEFAULT_SEARCH_URL_PREFIX, TAB_BAR_HEIGHT};
use crate::events::UiAction;

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

/// All mutable UI state consumed by `ChromeRenderer::render()` each frame.
pub struct ChromeState {
    pub tabs: Vec<TabInfo>,
    pub active_tab_id: Option<u64>,
    /// Text currently shown in the omnibox input field.
    pub omnibox_text: String,
    /// True while the user is editing the omnibox; prevents event-driven clobbering.
    pub omnibox_focused: bool,
    /// Tab whose URL was last copied into the omnibox.
    pub last_synced_tab_id: Option<u64>,
    /// URL that was last copied into the omnibox.
    pub last_synced_url: String,
    /// Whether the active tab is currently loading.
    pub is_loading: bool,
    /// RAM used by this process in megabytes (updated by sysinfo watchdog).
    pub ram_mb: u64,
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
    ) -> Self {
        let surface = instance
            .create_surface(Arc::clone(&window))
            .expect("create_surface failed");

        let size = window.inner_size();
        let caps = surface.get_capabilities(adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        // Clamp surface dimensions to the adapter's max texture size.
        let max_tex = adapter.limits().max_texture_dimension_2d;
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1).min(max_tex),
            height: size.height.max(1).min(max_tex),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
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

        Self {
            surface,
            device,
            queue,
            surface_config,
            renderer,
            egui_ctx,
            egui_state,
            favicon_textures: HashMap::new(),
            max_texture_dim: max_tex,
        }
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
        ctrl_l_pressed: bool,
    ) -> Vec<UiAction> {
        let mut actions: Vec<UiAction> = Vec::new();

        self.sync_favicon_textures(state);

        let mut raw_input = self.egui_state.take_egui_input(window);

        if ctrl_l_pressed {
            state.omnibox_focused = true;
            // Let the focused TextEdit process a synthetic Cmd/Ctrl+A so the
            // whole current URL is selected, matching browser omnibox behavior.
            raw_input.events.push(egui::Event::Key {
                key: egui::Key::A,
                physical_key: Some(egui::Key::A),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
        }

        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            self.build_ui(ctx, state, &mut actions, ctrl_l_pressed);
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
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.17,
                                g: 0.17,
                                b: 0.17,
                                a: 1.0,
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

    fn build_ui(
        &self,
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        ctrl_l_pressed: bool,
    ) {
        let tab_height = TAB_BAR_HEIGHT;
        let _chrome_height = CHROME_HEIGHT;

        // ── Tab Strip ───────────────────────────────────────────────────────
        egui::TopBottomPanel::top("tab_strip")
            .exact_height(tab_height)
            .frame(Frame::none().fill(Color32::from_rgb(30, 31, 35)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(5.0);

                    egui::ScrollArea::horizontal()
                        .id_salt("tab_scroll")
                        .show(ui, |ui| {
                            for tab in &state.tabs {
                                let is_active = state.active_tab_id == Some(tab.id);
                                let tab_id = tab.id;

                                let title = if tab.title.is_empty() {
                                    "New Tab".to_string()
                                } else if tab.title.chars().count() > 24 {
                                    let truncated: String = tab.title.chars().take(24).collect();
                                    format!("{}…", truncated)
                                } else {
                                    tab.title.clone()
                                };

                                let label_text = if tab.is_loading {
                                    format!("⟳ {}", title)
                                } else if tab.display_state == TabDisplayState::Hibernated {
                                    format!("💤 {}", title)
                                } else {
                                    title.clone()
                                };

                                let fill = if is_active {
                                    Color32::from_rgb(64, 66, 72)
                                } else {
                                    Color32::from_rgb(44, 45, 50)
                                };
                                let stroke = if is_active {
                                    Stroke::new(1.0_f32, Color32::from_rgb(88, 140, 220))
                                } else {
                                    Stroke::new(0.5_f32, Color32::from_rgb(62, 63, 68))
                                };

                                let frame_resp = Frame::none()
                                    .fill(fill)
                                    .rounding(Rounding::same(6.0))
                                    .stroke(stroke)
                                    .inner_margin(egui::Margin::symmetric(8.0, 0.0))
                                    .show(ui, |ui| {
                                        ui.set_max_height(tab_height - 4.0);
                                        ui.with_layout(
                                            Layout::left_to_right(Align::Center),
                                            |ui| {
                                                self.paint_favicon(ui, tab);

                                                let tab_btn = ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&label_text)
                                                            .color(Color32::from_rgb(235, 235, 238))
                                                            .size(13.0),
                                                    )
                                                    .sense(Sense::click()),
                                                );
                                                if tab_btn.clicked() {
                                                    actions.push(UiAction::SelectTab { tab_id });
                                                }
                                                tab_btn.on_hover_text(format!(
                                                    "{}\n{}",
                                                    tab.title, tab.url
                                                ));

                                                ui.add_space(4.0);

                                                let close = ui.add(
                                                    egui::Label::new(
                                                        RichText::new("×")
                                                            .color(Color32::from_rgb(150, 150, 155))
                                                            .size(15.0),
                                                    )
                                                    .sense(Sense::click()),
                                                );
                                                if close.hovered() {
                                                    ui.painter().rect_filled(
                                                        close.rect,
                                                        Rounding::same(4.0_f32),
                                                        Color32::from_rgba_unmultiplied(
                                                            255, 255, 255, 28,
                                                        ),
                                                    );
                                                }
                                                if close.clicked() {
                                                    actions.push(UiAction::CloseTab { tab_id });
                                                }
                                                close.on_hover_text("Close tab");
                                            },
                                        );
                                    });

                                if frame_resp.response.hovered() && !is_active {
                                    ui.painter().rect_filled(
                                        frame_resp.response.rect,
                                        Rounding::same(6.0_f32),
                                        Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                                    );
                                }

                                if is_active {
                                    let rect = frame_resp.response.rect;
                                    ui.painter().line_segment(
                                        [
                                            pos2(rect.left() + 7.0, rect.top() + 1.0),
                                            pos2(rect.right() - 7.0, rect.top() + 1.0),
                                        ],
                                        Stroke::new(2.0_f32, Color32::from_rgb(120, 175, 255)),
                                    );
                                }

                                ui.add_space(3.0);
                            }
                        });

                    let new_tab = ui.add(
                        egui::Label::new(RichText::new("+").color(Color32::WHITE).size(17.0))
                            .sense(Sense::click()),
                    );
                    if new_tab.clicked() {
                        actions.push(UiAction::NewTab { url: None });
                    }
                    new_tab.on_hover_text("New tab (Cmd/Ctrl+T)");
                });
            });

        // ── Toolbar (nav + omnibox + badges) ───────────────────────────────
        egui::TopBottomPanel::top("toolbar")
            .exact_height(TAB_BAR_HEIGHT)
            .frame(Frame::none().fill(Color32::from_rgb(34, 35, 39)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(6.0);

                    let active_tab = state
                        .tabs
                        .iter()
                        .find(|t| Some(t.id) == state.active_tab_id);
                    let can_back = active_tab.map(|t| t.can_go_back).unwrap_or(false);
                    let can_fwd = active_tab.map(|t| t.can_go_forward).unwrap_or(false);

                    let back = Self::nav_button(ui, can_back, true, "Back (Cmd/Ctrl+←)");
                    if back.clicked() && can_back {
                        if let Some(id) = state.active_tab_id {
                            actions.push(UiAction::GoBack { tab_id: id });
                        }
                    }

                    let forward = Self::nav_button(ui, can_fwd, false, "Forward (Cmd/Ctrl+→)");
                    if forward.clicked() && can_fwd {
                        if let Some(id) = state.active_tab_id {
                            actions.push(UiAction::GoForward { tab_id: id });
                        }
                    }

                    let (reload_label, is_loading_now, reload_tooltip) = if state.is_loading {
                        ("✕", true, "Stop loading (Esc)")
                    } else {
                        ("↻", false, "Reload (Cmd/Ctrl+R)")
                    };
                    let reload_btn = ui.add_sized(
                        vec2(28.0, 28.0),
                        egui::Button::new(
                            RichText::new(reload_label).color(Color32::WHITE).size(16.0),
                        )
                        .stroke(Stroke::NONE),
                    );
                    if reload_btn.clicked() {
                        if let Some(id) = state.active_tab_id {
                            if is_loading_now {
                                actions.push(UiAction::Stop { tab_id: id });
                            } else {
                                actions.push(UiAction::Reload {
                                    tab_id: id,
                                    ignore_cache: false,
                                });
                            }
                        }
                    }
                    reload_btn.on_hover_text(reload_tooltip);

                    ui.add_space(4.0);

                    let omnibox_width = (ui.available_width() - 160.0).max(160.0);
                    let omnibox_id = egui::Id::new("kasumi_omnibox");
                    if ctrl_l_pressed {
                        // Set focus before the widget processes this frame's
                        // synthetic select-all event.
                        ctx.memory_mut(|mem| mem.request_focus(omnibox_id));
                    }
                    let omnibox_resp = ui.add(
                        TextEdit::singleline(&mut state.omnibox_text)
                            .id(omnibox_id)
                            .desired_width(omnibox_width)
                            .hint_text("Search or enter address…")
                            .text_color(Color32::WHITE)
                            .frame(true),
                    );

                    state.omnibox_focused = omnibox_resp.has_focus();
                    let omnibox_resp =
                        omnibox_resp.on_hover_text("Search or enter address (Cmd/Ctrl+L)");

                    if omnibox_resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let url = resolve_omnibox(&state.omnibox_text);
                        if let Some(id) = state.active_tab_id {
                            actions.push(UiAction::Navigate { tab_id: id, url });
                        }
                        state.omnibox_focused = false;
                    }

                    ui.add_space(4.0);

                    let blocked: u32 = active_tab.map(|t| t.blocked_requests).unwrap_or(0);
                    ui.label(
                        RichText::new(format!("🛡 {}", blocked))
                            .color(Color32::from_rgb(80, 200, 120))
                            .size(12.0),
                    )
                    .on_hover_text(format!("Blocked requests: {}", blocked));

                    ui.add_space(4.0);

                    let hibernate_btn = ui.add_sized(
                        vec2(28.0, 28.0),
                        egui::Button::new(RichText::new("💤").color(Color32::WHITE).size(14.0))
                            .stroke(Stroke::NONE),
                    );
                    if hibernate_btn.clicked() {
                        actions.push(UiAction::HibernateAllBackground);
                    }
                    hibernate_btn.on_hover_text("Hibernate all background tabs (Alt+H)");

                    ui.add_space(4.0);

                    ui.label(
                        RichText::new(format!("RAM: {} MB", state.ram_mb))
                            .color(Color32::GRAY)
                            .size(11.0),
                    )
                    .on_hover_text("Browser-process RAM usage");

                    ui.add_space(4.0);
                });
            });

        // Fill the rest of the window with a transparent (zero-height) central panel.
        egui::CentralPanel::default()
            .frame(Frame::none().fill(Color32::TRANSPARENT))
            .show(ctx, |_ui| {});
    }

    /// Draw a native chevron icon so Back/Forward never depend on font glyphs.
    fn nav_button(ui: &mut Ui, enabled: bool, point_left: bool, tooltip: &str) -> Response {
        let (rect, response) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
        if !ui.is_rect_visible(rect) {
            return response.on_hover_text(tooltip);
        }

        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(68, 69, 75));
        }

        let color = if enabled {
            Color32::from_rgb(235, 235, 238)
        } else {
            Color32::from_rgb(105, 106, 112)
        };
        let center = rect.center();
        let tip_x = if point_left {
            center.x - 4.0
        } else {
            center.x + 4.0
        };
        let base_x = if point_left {
            center.x + 3.0
        } else {
            center.x - 3.0
        };
        let stroke = Stroke::new(1.8_f32, color);
        painter.line_segment(
            [pos2(tip_x, center.y), pos2(base_x, center.y - 5.5)],
            stroke,
        );
        painter.line_segment(
            [pos2(tip_x, center.y), pos2(base_x, center.y + 5.5)],
            stroke,
        );

        response.on_hover_text(tooltip)
    }

    /// Draw a cached favicon, or a neutral fallback dot if none is available.
    fn paint_favicon(&self, ui: &mut Ui, tab: &TabInfo) {
        let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
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

        ui.painter()
            .circle_filled(rect.center(), 4.0, Color32::from_rgb(120, 130, 145));
    }
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
    // but it must survive omnibox input unchanged.
    if trimmed.starts_with("view-source:") {
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
    use super::resolve_omnibox;

    #[test]
    fn test_resolve_omnibox() {
        assert_eq!(resolve_omnibox("https://github.com"), "https://github.com");
        assert_eq!(
            resolve_omnibox("github.com/rust-lang"),
            "https://github.com/rust-lang"
        );
        assert!(resolve_omnibox("hello world").contains("duckduckgo.com"));
        assert!(resolve_omnibox("rust async").contains("duckduckgo.com"));
        assert_eq!(
            resolve_omnibox("view-source:https://example.com"),
            "view-source:https://example.com"
        );
    }
}
