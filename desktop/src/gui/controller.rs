use crate::artix::CursorKind;
use crate::backends::DesktopUiBackend;
use crate::custom_event::RuffleEvent;
use crate::gui::movie::{MovieView, MovieViewRenderer};
use crate::gui::theme::ThemeController;
use crate::gui::{MENU_HEIGHT, RuffleGui};
use crate::player::{LaunchOptions, PlayerController};
use crate::preferences::GlobalPreferences;
use anyhow::anyhow;
use egui::{Context, FontData, FontDefinitions, ViewportId};
use fontdb::{Database, Family, Query, Source};
use ruffle_core::events::{ImeCursorArea, ImePurpose};
use ruffle_core::{Player, PlayerEvent};
use ruffle_frontend_utils::content::ContentDescriptor;
use ruffle_render_wgpu::backend::{
    WgpuRenderBackend, aqw_current_supersample, create_wgpu_instance, request_adapter_and_device,
};
use ruffle_render_wgpu::descriptors::Descriptors;
use ruffle_render_wgpu::utils::{format_list, get_backend_names};
use std::any::Any;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};
use url::Url;
use wgpu::SurfaceError;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, CustomCursor, ImePurpose as WinitImePurpose, Theme, Window};

use super::dialogs::export_bundle_dialog::ExportBundleDialogConfiguration;
use super::{DialogDescriptor, FilePicker};

/// Integration layer connecting wgpu+winit to egui.
pub struct GuiController {
    descriptors: Arc<Descriptors>,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    gui: RuffleGui,
    window: Arc<Window>,
    last_update: Instant,
    repaint_after: Duration,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    movie_view_renderer: Arc<MovieViewRenderer>,
    // Note that `window.get_inner_size` can change at any point on x11, even between two lines of code.
    // Use this instead.
    size: PhysicalSize<u32>,
    /// If this is set, we should not render the main menu.
    no_gui: bool,
    theme_controller: ThemeController,
    /// Artwork standing in for the system cursors, built once at startup.
    artix_cursors: Option<ArtixCursors>,
    /// The custom cursor currently pushed to the window. `set_cursor` on every
    /// frame flickers on Windows, so we only push it when it changes.
    applied_cursor: Option<CustomCursor>,
    /// egui pushes its own cursor only when its icon changes — and that clobbers
    /// ours, so we watch for it and re-apply.
    last_egui_cursor: Option<egui::CursorIcon>,
}

struct ArtixCursors {
    arrow: CustomCursor,
    hand: CustomCursor,
}

impl ArtixCursors {
    fn get(&self, kind: CursorKind) -> &CustomCursor {
        match kind {
            CursorKind::Arrow => &self.arrow,
            CursorKind::Hand => &self.hand,
        }
    }
}

impl GuiController {
    pub fn new(
        window: Arc<Window>,
        event_loop: EventLoopProxy<RuffleEvent>,
        preferences: GlobalPreferences,
        font_database: &Database,
        initial_movie_url: Option<Url>,
        no_gui: bool,
    ) -> anyhow::Result<Self> {
        let (instance, backend) = select_wgpu_backend(preferences.graphics_backends().into())?;
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(window.as_ref())?)
        }?;
        let (adapter, device, queue) = futures::executor::block_on(request_adapter_and_device(
            backend,
            &instance,
            Some(&surface),
            preferences.graphics_power_preference().into(),
        ))
        .map_err(|e| anyhow!(e.to_string()))?;
        let adapter_info = adapter.get_info();
        tracing::info!(
            "Using graphics API {} on {} (type: {:?})",
            adapter_info.backend.to_str(),
            adapter_info.name,
            adapter_info.device_type
        );
        let preferred_formats = [
            // by egui
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8Unorm,
        ];
        let supported_formats = surface.get_capabilities(&adapter).formats;
        let surface_format = preferred_formats
            .iter()
            .find(|format| supported_formats.contains(format))
            .copied()
            .unwrap_or_else(|| {
                supported_formats
                    .first()
                    .copied()
                    .expect("At least one format should be supported")
            });
        tracing::info!("Using surface format {:?}", surface_format);
        let size = window.inner_size();
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: surface_format,
                width: size.width,
                height: size.height,
                present_mode: Default::default(),
                desired_maximum_frame_latency: 2,
                alpha_mode: Default::default(),
                view_formats: Default::default(),
            },
        );
        let descriptors = Descriptors::new(instance, adapter, device, queue);
        let egui_ctx = Context::default();

        let theme_controller = futures::executor::block_on(ThemeController::new(
            window.clone(),
            preferences.clone(),
            egui_ctx.clone(),
        ));
        let mut egui_winit = egui_winit::State::new(
            egui_ctx,
            ViewportId::ROOT,
            window.as_ref(),
            None,
            None,
            None,
        );
        egui_winit.set_max_texture_side(descriptors.limits.max_texture_dimension_2d as usize);

        let movie_view_renderer = Arc::new(MovieViewRenderer::new(
            &descriptors.device,
            surface_format,
            window.fullscreen().is_none() && !no_gui,
            size.height,
            window.scale_factor(),
        ));
        let egui_renderer = egui_wgpu::Renderer::new(
            &descriptors.device,
            surface_format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );
        let descriptors = Arc::new(descriptors);
        let gui = RuffleGui::new(
            Arc::downgrade(&window),
            event_loop,
            initial_movie_url.map(|url| ContentDescriptor {
                url,
                root_content_path: None,
            }),
            LaunchOptions::from(&preferences),
            preferences.clone(),
        );
        let system_fonts = load_system_fonts(font_database, preferences.language());
        egui_winit.egui_ctx().set_fonts(system_fonts);

        egui_extras::install_image_loaders(egui_winit.egui_ctx());

        Ok(Self {
            descriptors,
            egui_winit,
            egui_renderer,
            gui,
            window,
            last_update: Instant::now(),
            repaint_after: Duration::ZERO,
            surface,
            surface_format,
            movie_view_renderer,
            size,
            no_gui,
            theme_controller,
            artix_cursors: None,
            applied_cursor: None,
            last_egui_cursor: None,
        })
    }

    /// Build the custom cursors. Separate from `new` because creating them needs
    /// the `ActiveEventLoop`, which only the application handler holds.
    pub fn init_custom_cursors(&mut self, event_loop: &ActiveEventLoop) {
        let Some(set) = crate::artix::custom_cursors() else {
            return;
        };
        let build = |art: &crate::artix::CursorArt| {
            match CustomCursor::from_rgba(
                art.rgba.to_vec(),
                art.size,
                art.size,
                art.hotspot_x,
                art.hotspot_y,
            ) {
                Ok(source) => Some(event_loop.create_custom_cursor(source)),
                Err(e) => {
                    tracing::warn!("Custom cursor rejected, keeping the system one: {e}");
                    None
                }
            }
        };
        if let (Some(arrow), Some(hand)) = (build(&set.arrow), build(&set.hand)) {
            self.artix_cursors = Some(ArtixCursors { arrow, hand });
        }
    }

    pub fn set_theme(&self, theme: Theme) {
        self.theme_controller.set_theme(theme);
    }

    pub fn descriptors(&self) -> &Arc<Descriptors> {
        &self.descriptors
    }

    pub fn file_picker(&self) -> FilePicker {
        self.gui.dialogs.file_picker()
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width > 0 && size.height > 0 {
            self.size = size;
            self.reconfigure_surface();
        }
    }

    pub fn reconfigure_surface(&self) {
        self.surface.configure(
            &self.descriptors.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.surface_format,
                width: self.size.width,
                height: self.size.height,
                present_mode: Default::default(),
                desired_maximum_frame_latency: 2,
                alpha_mode: Default::default(),
                view_formats: Default::default(),
            },
        );
        self.movie_view_renderer.update_resolution(
            &self.descriptors,
            self.window.fullscreen().is_none() && !self.no_gui,
            self.size.height,
            self.window.scale_factor(),
        );
    }

    #[must_use]
    pub fn handle_event(&mut self, event: &WindowEvent) -> bool {
        if let WindowEvent::Resized(size) = &event {
            self.resize(*size);
        }

        if let WindowEvent::ThemeChanged(theme) = &event {
            self.set_theme(*theme);
        }

        if matches!(
            &event,
            WindowEvent::KeyboardInput {
                event: winit::event::KeyEvent {
                    logical_key: Key::Named(NamedKey::Tab),
                    ..
                },
                ..
            }
        ) {
            // Prevent egui from consuming the Tab key.
            return false;
        }

        let response = self.egui_winit.on_window_event(&self.window, event);
        if response.repaint {
            self.window.request_redraw();
        }
        response.consumed
    }

    pub fn close_movie(&mut self, player: &mut PlayerController) {
        player.destroy();
        self.gui.on_player_destroyed();
    }

    pub fn create_movie(
        &mut self,
        player: &mut PlayerController,
        opt: LaunchOptions,
        content_descriptor: ContentDescriptor,
    ) {
        tracing::info!("Opening {}", content_descriptor.describe());

        self.close_movie(player);
        let movie_view = MovieView::new(
            self.movie_view_renderer.clone(),
            &self.descriptors.device,
            self.size.width,
            self.size.height,
        );
        player.create(&opt, &content_descriptor, movie_view);
        self.gui.on_player_created(
            opt,
            content_descriptor,
            player
                .get()
                .expect("Player must exist after being created."),
        );
    }

    pub fn height_offset(&self) -> f64 {
        if self.window.fullscreen().is_some() || self.no_gui {
            0.0
        } else {
            MENU_HEIGHT as f64 * self.window.scale_factor()
        }
    }

    /// Forward CRT barrel warp in movie-area window coordinates. The CRT
    /// present shader shows the content of warp(uv) at screen uv, so a
    /// click at uv is aiming at warp(uv) — the SAME function (and shared
    /// strength constant) maps mouse coordinates. No-op when the filter is
    /// off.
    fn crt_warp_window_position(&self, x: f64, y: f64) -> (f64, f64) {
        let k = f64::from(ruffle_render::backend::aqw_crt_warp_strength());
        if k <= 0.0 || !ruffle_render::backend::aqw_crt_filter_enabled() {
            return (x, y);
        }
        let w = f64::from(self.size.width);
        let h = f64::from(self.size.height) - self.height_offset();
        if w <= 0.0 || h <= 0.0 {
            return (x, y);
        }
        let cx = x / w * 2.0 - 1.0;
        let cy = y / h * 2.0 - 1.0;
        let f = 1.0 + k * (cx * cx + cy * cy);
        (((cx * f) + 1.0) * 0.5 * w, ((cy * f) + 1.0) * 0.5 * h)
    }

    /// Inverse of [`Self::crt_warp_window_position`] (fixed-point
    /// iteration; the warp is gentle so two rounds converge well below a
    /// pixel).
    fn crt_unwarp_window_position(&self, x: f64, y: f64) -> (f64, f64) {
        let k = f64::from(ruffle_render::backend::aqw_crt_warp_strength());
        if k <= 0.0 || !ruffle_render::backend::aqw_crt_filter_enabled() {
            return (x, y);
        }
        let w = f64::from(self.size.width);
        let h = f64::from(self.size.height) - self.height_offset();
        if w <= 0.0 || h <= 0.0 {
            return (x, y);
        }
        let tx = x / w * 2.0 - 1.0;
        let ty = y / h * 2.0 - 1.0;
        let mut cx = tx;
        let mut cy = ty;
        for _ in 0..3 {
            let f = 1.0 + k * (cx * cx + cy * cy);
            cx = tx / f;
            cy = ty / f;
        }
        ((cx + 1.0) * 0.5 * w, (cy + 1.0) * 0.5 * h)
    }

    pub fn window_to_movie_position(&self, position: PhysicalPosition<f64>) -> (f64, f64) {
        // When the renderer supersamples, it reports an N× viewport to the player, so
        // stage hit-testing expects window coordinates scaled up by the same N.
        // (No-op at N=1 — click-to-move in AQW must land on the right cell.)
        // Reads the factor currently in effect — the renderer may gate SSAA off
        // for large windows — so clicks track whatever is actually rendering.
        let ss = f64::from(aqw_current_supersample());
        let (wx, wy) = self.crt_warp_window_position(position.x, position.y - self.height_offset());
        (wx * ss, wy * ss)
    }

    pub fn movie_to_window_position(&self, x: f64, y: f64) -> PhysicalPosition<f64> {
        let ss = f64::from(aqw_current_supersample());
        let (ux, uy) = self.crt_unwarp_window_position(x / ss, y / ss);
        PhysicalPosition::new(ux, uy + self.height_offset())
    }

    pub fn render(&mut self, mut player: Option<MutexGuard<Player>>) {
        let surface_texture = match self.surface.get_current_texture() {
            Ok(surface_texture) => surface_texture,
            Err(e @ (SurfaceError::Lost | SurfaceError::Outdated)) => {
                // Reconfigure the surface if lost or outdated.
                // Some sources suggest ignoring `Outdated` and waiting for the next frame,
                // but I suspect this advice is related explicitly to resizing,
                // because the future resize event will reconfigure the surface.
                // However, resizing is not the only possible reason for the surface
                // to become outdated (resolution / refresh rate change, some internal
                // platform-specific reasons, wgpu bugs?).
                // Testing on Vulkan shows that reconfiguring the surface works in that case.
                tracing::warn!("Surface became unavailable: {:?}, reconfiguring", e);
                self.reconfigure_surface();
                return;
            }
            Err(e @ SurfaceError::Timeout) => {
                // An operation related to the surface took too long to complete.
                // This error may happen due to many reasons (GPU overload, GPU driver bugs, etc.),
                // the best thing we can do is skip a frame and wait.
                tracing::warn!("Surface became unavailable: {:?}, skipping a frame", e);
                return;
            }
            Err(e @ (SurfaceError::OutOfMemory | SurfaceError::Other)) => {
                // Vulkan can return a generic acquire failure from inside a
                // window-message callback (modal resize/move loop, DPI change)
                // that clears by the next frame, and OOM here is as transient
                // as it is in the AQW render path (the VRAM valve drains it).
                // Neither is worth killing the game over: treat both like
                // Lost/Outdated — reconfigure and skip this frame.
                tracing::error!("Surface acquire failed: {e:?}; reconfiguring and skipping a frame");
                self.reconfigure_surface();
                return;
            }
        };

        let raw_input = self.egui_winit.take_egui_input(&self.window);
        let show_menu = self.window.fullscreen().is_none() && !self.no_gui;
        let mut full_output = self.egui_winit.egui_ctx().run(raw_input, |context| {
            self.gui.update(
                context,
                show_menu,
                player.as_deref_mut(),
                if show_menu {
                    MENU_HEIGHT as f64 * self.window.scale_factor()
                } else {
                    0.0
                },
            );
        });
        self.repaint_after = full_output
            .viewport_output
            .get(&ViewportId::ROOT)
            .expect("Root viewport must exist")
            .repaint_delay;

        // If we're not in a UI, tell egui which cursor we prefer to use instead
        let mut desired_cursor = None;
        if !self.egui_winit.egui_ctx().wants_pointer_input()
            && let Some(player) = player.as_deref()
        {
            let ui = <dyn Any>::downcast_ref::<DesktopUiBackend>(player.ui())
                .unwrap_or_else(|| panic!("UI Backend should be DesktopUiBackend"));
            full_output.platform_output.cursor_icon = ui.cursor();
            desired_cursor = self
                .artix_cursors
                .as_ref()
                .zip(ui.artix_cursor())
                .map(|(cursors, kind)| cursors.get(kind).clone());
        }

        let egui_cursor = full_output.platform_output.cursor_icon;
        let egui_pushed_cursor = self.last_egui_cursor != Some(egui_cursor);
        self.last_egui_cursor = Some(egui_cursor);
        self.egui_winit
            .handle_platform_output(&self.window, full_output.platform_output);

        // egui just pushed its own icon if it changed, so re-apply ours over it.
        // On release we hand the cursor back: either egui already pushed the icon
        // it wants this frame, or it didn't change, in which case it's the
        // default it drew the movie area with.
        if desired_cursor != self.applied_cursor || (desired_cursor.is_some() && egui_pushed_cursor)
        {
            match &desired_cursor {
                Some(cursor) => self.window.set_cursor(cursor.clone()),
                None if !egui_pushed_cursor => self.window.set_cursor(CursorIcon::Default),
                None => {}
            }
            self.applied_cursor = desired_cursor;
        }

        let clipped_primitives = self
            .egui_winit
            .egui_ctx()
            .tessellate(full_output.shapes, full_output.pixels_per_point);

        let scale_factor = self.window.scale_factor() as f32;
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point: scale_factor,
        };

        let mut encoder =
            self.descriptors
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("egui encoder"),
                });

        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer.update_texture(
                &self.descriptors.device,
                &self.descriptors.queue,
                *id,
                image_delta,
            );
        }

        let mut command_buffers = self.egui_renderer.update_buffers(
            &self.descriptors.device,
            &self.descriptors.queue,
            &mut encoder,
            &clipped_primitives,
            &screen_descriptor,
        );

        let movie_view = if let Some(player) = player.as_deref_mut() {
            let renderer =
                <dyn Any>::downcast_ref::<WgpuRenderBackend<MovieView>>(player.renderer_mut())
                    .expect("Renderer must be correct type");
            Some(renderer.target())
        } else {
            None
        };

        {
            let surface_view = surface_texture.texture.create_view(&Default::default());

            let mut render_pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    label: Some("egui_render"),
                    ..Default::default()
                })
                .forget_lifetime();

            if let Some(movie_view) = movie_view {
                movie_view.render(&self.movie_view_renderer, &mut render_pass);
            }

            self.egui_renderer
                .render(&mut render_pass, &clipped_primitives, &screen_descriptor);
        }

        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        command_buffers.push(encoder.finish());
        self.descriptors.queue.submit(command_buffers);
        self.window.pre_present_notify();
        surface_texture.present();
    }

    pub fn show_context_menu(
        &mut self,
        menu: Vec<ruffle_core::ContextMenuItem>,
        close_event: PlayerEvent,
    ) {
        self.gui.show_context_menu(menu, close_event);
    }

    pub fn is_context_menu_visible(&self) -> bool {
        self.gui.is_context_menu_visible()
    }

    pub fn needs_render(&self) -> bool {
        Instant::now().duration_since(self.last_update) >= self.repaint_after
    }

    pub fn show_open_dialog(&mut self) {
        self.gui.dialogs.open_file_advanced()
    }

    pub fn open_dialog(&mut self, dialog_event: DialogDescriptor) {
        self.gui.dialogs.open_dialog(dialog_event);
    }

    pub fn set_ime_allowed(&self, allowed: bool) {
        self.window.set_ime_allowed(allowed);
    }

    pub fn set_ime_purpose(&self, purpose: ImePurpose) {
        self.window.set_ime_purpose(match purpose {
            ImePurpose::Standard => WinitImePurpose::Normal,
            ImePurpose::Password => WinitImePurpose::Password,
        });
    }

    pub fn set_ime_cursor_area(&self, cursor_area: ImeCursorArea) {
        self.window.set_ime_cursor_area(
            self.movie_to_window_position(cursor_area.x, cursor_area.y),
            PhysicalSize::new(cursor_area.width, cursor_area.height),
        );
    }

    pub fn export_bundle(&mut self) {
        let Some(content_descriptor) = self.gui.dialogs.saved_content_descriptor() else {
            return;
        };

        let launch_options = self.gui.dialogs.saved_launch_options();
        let player_options = launch_options.player.clone();
        self.gui
            .dialogs
            .open_dialog(DialogDescriptor::ExportBundle(Box::new(
                ExportBundleDialogConfiguration::new(content_descriptor, player_options),
            )));
        self.gui.on_player_destroyed();
    }
}

fn select_wgpu_backend(
    preferred_backends: wgpu::Backends,
) -> anyhow::Result<(wgpu::Instance, wgpu::Backends)> {
    for backend in preferred_backends.iter() {
        if let Some(instance) = try_wgpu_backend(backend) {
            tracing::info!(
                "Using preferred backend {}",
                format_list(&get_backend_names(backend), "and")
            );
            return Ok((instance, backend));
        }
    }

    tracing::warn!(
        "Preferred backend(s) of {} not available; falling back to any",
        format_list(&get_backend_names(preferred_backends), "or")
    );

    for backend in wgpu::Backends::all() - preferred_backends {
        if let Some(instance) = try_wgpu_backend(backend) {
            tracing::info!(
                "Using fallback backend {}",
                format_list(&get_backend_names(backend), "and")
            );
            return Ok((instance, backend));
        }
    }

    Err(anyhow!(
        "No compatible graphics backends of any kind were available"
    ))
}

fn try_wgpu_backend(backend: wgpu::Backends) -> Option<wgpu::Instance> {
    let instance = create_wgpu_instance(backend, wgpu::BackendOptions::default());
    if instance.enumerate_adapters(backend).is_empty() {
        None
    } else {
        Some(instance)
    }
}

// Load fallback fonts
fn load_system_fonts(
    font_database: &Database,
    locale: unic_langid::LanguageIdentifier,
) -> egui::FontDefinitions {
    let mut fd: FontDefinitions = egui::FontDefinitions::default();

    let lang = locale.language.as_str();
    let is_ja = lang == "ja";
    let is_ko = lang == "ko";
    let is_zh = lang == "zh";
    let is_sc = is_zh && locale.to_string().as_str() == "zh-CN";
    let is_tc = is_zh && !is_sc;

    let mut queries: PrioritizedQueries = Vec::new();

    // The main font
    queries.push((1, vec![Family::SansSerif]));

    // Pan-CJK fonts
    queries.push((
        2,
        vec![
            Family::Name("Noto Sans CJK"),     // Open font
            Family::Name("Source Han Sans"),   // Open font, same as Noto Sans CJK
            Family::Name("WenQuanYi Zen Hei"), // Open font
            Family::Name("Arial Unicode MS"),  // MacOS
        ],
    ));

    // Korean
    queries.push((
        3 + if is_ko { 0 } else { 1 },
        vec![
            Family::Name("Noto Sans CJK KR"), // Open font
            Family::Name("Malgun Gothic"),    // Windows
        ],
    ));

    // Japanese
    queries.push((
        3 + if is_ja { 0 } else { 1 },
        vec![
            Family::Name("Noto Sans CJK JP"), // Open font
            Family::Name("MS UI Gothic"),     // Windows
        ],
    ));

    // Chinese Simplified
    queries.push((
        3 + if is_sc { 0 } else { 1 },
        vec![
            Family::Name("Noto Sans CJK SC"), // Open font
            Family::Name("Microsoft YaHei"),  // Windows
        ],
    ));

    // Chinese Traditional
    queries.push((
        3 + if is_tc { 0 } else { 1 },
        vec![
            Family::Name("Noto Sans CJK TC"),   // Open font
            Family::Name("Microsoft JhengHei"), // Windows
        ],
    ));

    // Hebrew
    queries.push((
        4,
        vec![
            Family::Name("Noto Sans Hebrew"), // Open font
            Family::Name("Tahoma"),           // Windows
        ],
    ));

    // Arabic
    queries.push((
        5,
        vec![
            Family::Name("Noto Sans Arabic"), // Open font
            Family::Name("Tahoma"),           // Windows
        ],
    ));

    // Thai
    queries.push((
        6,
        vec![
            Family::Name("Noto Sans Thai"), // Open font
            Family::Name("Tahoma"),         // Windows
        ],
    ));

    register_family(
        font_database,
        &mut fd,
        egui::FontFamily::Proportional,
        queries,
    );

    fd
}

type FamilyQuery<'a> = Vec<Family<'a>>;
type PrioritizedQueries<'a> = Vec<(usize, FamilyQuery<'a>)>;

fn register_family(
    font_database: &Database,
    fd: &mut FontDefinitions,
    family: egui::FontFamily,
    mut queries: PrioritizedQueries<'_>,
) {
    queries.sort_by_key(|(priority, _)| *priority);
    for (_, query) in queries {
        register_family_font(font_database, fd, family.clone(), &query);
    }
}

fn register_family_font(
    font_database: &Database,
    fd: &mut FontDefinitions,
    family: egui::FontFamily,
    query: &FamilyQuery<'_>,
) {
    let (name, fontdata) = match load_system_font(font_database, query) {
        Ok((name, fontdata)) => (name, fontdata),
        Err(e) => {
            tracing::warn!("Failed to register {query:?} as {family}: {e}");
            return;
        }
    };

    tracing::debug!("Registering font {name} as {family}");

    fd.font_data.insert(name.clone(), fontdata.into());
    fd.families.entry(family).or_default().push(name);
}

fn load_system_font(
    font_database: &Database,
    families: &Vec<Family<'_>>,
) -> anyhow::Result<(String, FontData)> {
    let system_unicode_fonts = Query {
        families,
        ..Query::default()
    };

    let id = font_database
        .query(&system_unicode_fonts)
        .ok_or(anyhow!("no unicode fonts found!"))?;
    let (name, src, index) = font_database
        .face(id)
        .map(|f| (f.post_script_name.clone(), f.source.clone(), f.index))
        .expect("id not found in font database");

    let mut fontdata = match src {
        Source::File(path) | Source::SharedFile(path, _) => {
            let data = mmap_system_font(&path)?;

            // egui accepts only static data, so we have to leak mmapped fonts.
            // This is acceptable, as we're doing it only once.
            let data = Box::leak(Box::new(data));

            egui::FontData::from_static(data)
        }
        Source::Binary(bin) => {
            let data = bin.as_ref().as_ref().to_vec();
            egui::FontData::from_owned(data)
        }
    };
    fontdata.index = index;

    Ok((name, fontdata))
}

fn mmap_system_font(path: &Path) -> anyhow::Result<memmap2::Mmap> {
    let file = File::open(path).map_err(|e| anyhow!("Couldn't open font file at {path:?}: {e}"))?;

    // SAFETY: We have to assume that the font file won't change.
    // This assumption is realistic, as we're using system fonts only.
    let mmap = unsafe { memmap2::Mmap::map(&file) };

    let mmap = mmap.map_err(|e| anyhow!("Failed to mmap font file at {path:?}: {e}"))?;
    Ok(mmap)
}
