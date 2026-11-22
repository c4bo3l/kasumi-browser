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

use std::sync::Arc;

use egui::{Align, Color32, Frame, Layout, Rounding, Stroke, TextEdit};
use egui_wgpu::ScreenDescriptor;

use crate::config::{CHROME_HEIGHT, DEFAULT_SEARCH_URL_PREFIX, TAB_BAR_HEIGHT};
use crate::events::UiAction;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Snapshot of a single tab's metadata used by the chrome renderer.
/// (Cheap to clone — lives only for the duration of a render call.)
#[derive(Debug, Clone, PartialEq)]
pub enum TabDisplayState {
    Active,
    Background,
    Hibernated,
}

#[derive(Debug, Clone)]
pub struct TabInfo {
    pub id: u64,
    pub title: String,
    pub is_loading: bool,
    pub display_state: TabDisplayState,
    pub blocked_requests: u32,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

/// All mutable UI state consumed by `ChromeRenderer::render()` each frame.
pub struct ChromeState {
    pub tabs: Vec<TabInfo>,
    pub active_tab_id: Option<u64>,
    /// Text currently shown in the omnibox input field.
    pub omnibox_text: String,
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
    /// Whether the omnibox is currently focused (so Ctrl+L can grab it).
    omnibox_focused: bool,
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
        // On Intel Iris (this machine) the Metal backend reports max 2048,
        // while Retina physical pixels are 2560×1600 — so we must clamp.
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

        // egui_winit::State handles input translation.
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
            omnibox_focused: false,
            max_texture_dim: max_tex,
        }
    }

    /// Forward a winit `WindowEvent` to egui for input processing.
    /// Returns `true` if egui consumed the event (caller should not forward to CEF).
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
    ///
    /// `state` is consumed/mutated (omnibox text may be updated by typing).
    pub fn render(
        &mut self,
        window: &winit::window::Window,
        state: &mut ChromeState,
        ctrl_l_pressed: bool,
    ) -> Vec<UiAction> {
        let mut actions: Vec<UiAction> = Vec::new();

        // --- Gather raw input from winit ---
        let raw_input = self.egui_state.take_egui_input(window);

        // --- Focus omnibox on Ctrl+L ---
        if ctrl_l_pressed {
            self.omnibox_focused = true;
        }

        // --- Run egui UI ---
        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            Self::build_ui(ctx, state, &mut actions, ctrl_l_pressed);
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output.clone());

        // --- Render egui to wgpu ---
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

        // Update buffers
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

    // -----------------------------------------------------------------------
    // UI construction
    // -----------------------------------------------------------------------

    fn build_ui(
        ctx: &egui::Context,
        state: &mut ChromeState,
        actions: &mut Vec<UiAction>,
        ctrl_l_pressed: bool,
    ) {
        let scale = ctx.pixels_per_point();
        let tab_height = TAB_BAR_HEIGHT;
        let _chrome_height = CHROME_HEIGHT;

        // ── Tab Strip ───────────────────────────────────────────────────────
        egui::TopBottomPanel::top("tab_strip")
            .exact_height(tab_height)
            .frame(Frame::none().fill(Color32::from_rgb(40, 40, 40)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(4.0);

                    egui::ScrollArea::horizontal()
                        .id_salt("tab_scroll")
                        .show(ui, |ui| {
                            for tab in &state.tabs {
                                let is_active = state.active_tab_id == Some(tab.id);

                                // Tab background
                                let tab_bg = if is_active {
                                    Color32::from_rgb(60, 60, 60)
                                } else {
                                    Color32::from_rgb(45, 45, 45)
                                };

                                let tab_id = tab.id;
                                let title = if tab.title.is_empty() {
                                    "New Tab".to_string()
                                } else if tab.title.chars().count() > 24 {
                                    let truncated: String = tab.title.chars().take(24).collect();
                                    format!("{}…", truncated)
                                } else {
                                    tab.title.clone()
                                };

                                // Build label with state badges
                                let label_text = if tab.is_loading {
                                    format!("⟳ {}", title)
                                } else if tab.display_state == TabDisplayState::Hibernated {
                                    format!("💤 {}", title)
                                } else {
                                    title.clone()
                                };

                                Frame::none()
                                    .fill(tab_bg)
                                    .rounding(Rounding::same(4.0))
                                    .inner_margin(egui::Margin::symmetric(8.0, 0.0))
                                    .show(ui, |ui| {
                                        ui.set_max_height(tab_height - 4.0);
                                        ui.with_layout(
                                            Layout::left_to_right(Align::Center),
                                            |ui| {
                                                let tab_btn = ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&label_text)
                                                            .color(Color32::WHITE)
                                                            .size(13.0),
                                                    )
                                                    .sense(egui::Sense::click()),
                                                );
                                                if tab_btn.clicked() {
                                                    actions.push(UiAction::SelectTab { tab_id });
                                                }

                                                // Close button
                                                let close = ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(" ×")
                                                            .color(Color32::GRAY)
                                                            .size(14.0),
                                                    )
                                                    .sense(egui::Sense::click()),
                                                );
                                                if close.clicked() {
                                                    actions.push(UiAction::CloseTab { tab_id });
                                                }
                                            },
                                        );
                                    });

                                ui.add_space(2.0);
                            }
                        });

                    // New tab button
                    let new_tab = ui.add(
                        egui::Label::new(
                            egui::RichText::new(" + ").color(Color32::WHITE).size(16.0),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if new_tab.clicked() {
                        actions.push(UiAction::NewTab { url: None });
                    }
                    let _ = scale; // suppress unused warning
                });
            });

        // ── Toolbar (nav + omnibox + badges) ───────────────────────────────
        egui::TopBottomPanel::top("toolbar")
            .exact_height(TAB_BAR_HEIGHT) // second 38 px
            .frame(Frame::none().fill(Color32::from_rgb(32, 32, 32)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(6.0);

                    // ← Back
                    let can_back = state
                        .tabs
                        .iter()
                        .find(|t| Some(t.id) == state.active_tab_id)
                        .map(|t| t.can_go_back)
                        .unwrap_or(false);
                    let back_btn = ui.add_enabled(
                        can_back,
                        egui::Button::new(
                            egui::RichText::new("←").color(Color32::WHITE).size(16.0),
                        )
                        .stroke(Stroke::NONE),
                    );
                    if back_btn.clicked() {
                        if let Some(id) = state.active_tab_id {
                            actions.push(UiAction::GoBack { tab_id: id });
                        }
                    }

                    // → Forward
                    let can_fwd = state
                        .tabs
                        .iter()
                        .find(|t| Some(t.id) == state.active_tab_id)
                        .map(|t| t.can_go_forward)
                        .unwrap_or(false);
                    let fwd_btn = ui.add_enabled(
                        can_fwd,
                        egui::Button::new(
                            egui::RichText::new("→").color(Color32::WHITE).size(16.0),
                        )
                        .stroke(Stroke::NONE),
                    );
                    if fwd_btn.clicked() {
                        if let Some(id) = state.active_tab_id {
                            actions.push(UiAction::GoForward { tab_id: id });
                        }
                    }

                    // ↻ Reload / ✕ Stop
                    let (reload_label, is_loading_now) = if state.is_loading {
                        ("✕", true)
                    } else {
                        ("↻", false)
                    };
                    let reload_btn = ui.add(
                        egui::Button::new(
                            egui::RichText::new(reload_label)
                                .color(Color32::WHITE)
                                .size(16.0),
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

                    ui.add_space(4.0);

                    // ── Omnibox ──────────────────────────────────────────
                    let omnibox_resp = ui.add(
                        TextEdit::singleline(&mut state.omnibox_text)
                            .desired_width(ui.available_width() - 160.0)
                            .hint_text("Search or enter address…")
                            .text_color(Color32::WHITE)
                            .frame(true),
                    );

                    // Focus omnibox on Ctrl+L
                    if ctrl_l_pressed {
                        omnibox_resp.request_focus();
                    }

                    // Navigate on Enter
                    if omnibox_resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let url = resolve_omnibox(&state.omnibox_text);
                        if let Some(id) = state.active_tab_id {
                            actions.push(UiAction::Navigate { tab_id: id, url });
                        }
                    }

                    ui.add_space(4.0);

                    // 🛡 Blocked count badge
                    let blocked: u32 = state
                        .tabs
                        .iter()
                        .find(|t| Some(t.id) == state.active_tab_id)
                        .map(|t| t.blocked_requests)
                        .unwrap_or(0);
                    ui.label(
                        egui::RichText::new(format!("🛡 {}", blocked))
                            .color(Color32::from_rgb(80, 200, 120))
                            .size(12.0),
                    );

                    ui.add_space(4.0);

                    // 💤 Hibernate all background tabs
                    let hibernate_btn = ui.add(
                        egui::Button::new(
                            egui::RichText::new("💤").color(Color32::WHITE).size(14.0),
                        )
                        .stroke(Stroke::NONE),
                    );
                    if hibernate_btn.clicked() {
                        actions.push(UiAction::HibernateAllBackground);
                    }

                    ui.add_space(4.0);

                    // RAM readout
                    ui.label(
                        egui::RichText::new(format!("RAM: {} MB", state.ram_mb))
                            .color(Color32::GRAY)
                            .size(11.0),
                    );

                    ui.add_space(4.0);
                });
            });

        // Fill the rest of the window with a transparent (zero-height) central panel
        // so egui doesn't complain about missing central panel.
        egui::CentralPanel::default()
            .frame(Frame::none().fill(Color32::TRANSPARENT))
            .show(ctx, |_ui| {
                // CEF renders here via native child window — egui does nothing.
            });
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

    // 1. Valid URL (includes scheme)?
    if url::Url::parse(trimmed).is_ok() {
        return trimmed.to_string();
    }

    // 2. No spaces and contains a dot → assume bare domain / path
    if !trimmed.contains(' ') && trimmed.contains('.') {
        return format!("https://{}", trimmed);
    }

    // 3. Encode as DuckDuckGo search
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
    }
}
