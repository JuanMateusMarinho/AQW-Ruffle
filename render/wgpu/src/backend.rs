use crate::buffer_builder::BufferBuilder;
use crate::buffer_pool::{BufferPool, TexturePool};
use crate::context3d::WgpuContext3D;
use crate::dynamic_transforms::DynamicTransforms;
use crate::filters::FilterSource;
use crate::mesh::{CommonGradient, Mesh, PendingDraw};
use crate::pixel_bender::{ShaderMode, run_pixelbender_shader_impl};
use crate::surface::{LayerRef, Surface};
use crate::target::{MaybeOwnedBuffer, TextureTarget};
use crate::target::{RenderTargetFrame, TextureBufferInfo};
use crate::utils::{BufferDimensions, run_copy_pipeline};
use crate::{
    Descriptors, Error, QueueSyncHandle, RenderTarget, SwapChainTarget, Texture, as_texture,
    format_list, get_backend_names,
};
use image::imageops::FilterType;
use ruffle_render::backend::{
    BitmapCacheEntry, Context3D, Context3DProfile, PixelBenderOutput, PixelBenderTarget,
};
use ruffle_render::backend::{RenderBackend, ShapeHandle, ViewportDimensions};
use ruffle_render::bitmap::{
    Bitmap, BitmapFormat, BitmapHandle, BitmapSource, PixelRegion, RgbaBufRead, SyncHandle,
};
use ruffle_render::commands::CommandList;
use ruffle_render::error::Error as BitmapError;
use ruffle_render::filters::Filter;
use ruffle_render::pixel_bender::{PixelBenderShader, PixelBenderShaderHandle};
use ruffle_render::pixel_bender_support::PixelBenderShaderArgument;
use ruffle_render::quality::StageQuality;
use ruffle_render::shape_utils::DistilledShape;
use ruffle_render::tessellator::ShapeTessellator;
use std::any::Any;
use std::borrow::Cow;
use std::cell::Cell;
use std::num::NonZeroU32;
use std::sync::{Arc, OnceLock};
use swf::Color;
use tracing::instrument;
use wgpu::SubmissionIndex;

fn aqw_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RUFFLE_AQW_DIAGNOSTICS").is_some())
}

/// Process GPU-memory usage and OS-granted budget, in bytes, sampled via DXGI
/// (`IDXGIAdapter3::QueryVideoMemoryInfo`) and cached for about a second.
/// DXGI reports this regardless of which API renders (Vulkan included), and
/// per process, which is exactly the number the OS uses to decide when to
/// start demoting our textures to system memory.
#[cfg(windows)]
mod gpu_memory {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static CACHE: Mutex<Option<(Instant, Option<(u64, u64)>)>> = Mutex::new(None);

    pub fn query_cached() -> Option<(u64, u64)> {
        let mut guard = CACHE.lock().expect("gpu_memory cache poisoned");
        if let Some((at, value)) = *guard
            && at.elapsed() < Duration::from_secs(1)
        {
            return value;
        }
        let value = query();
        *guard = Some((Instant::now(), value));
        value
    }

    fn query() -> Option<(u64, u64)> {
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
            IDXGIAdapter3, IDXGIFactory1,
        };
        use windows::core::Interface;

        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            // The adapter we render on is the one where this process has the
            // most memory in use (relevant on hybrid laptops).
            let mut best: Option<(u64, u64)> = None;
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                index += 1;
                let Ok(adapter3) = adapter.cast::<IDXGIAdapter3>() else {
                    continue;
                };
                let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                if adapter3
                    .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                    .is_ok()
                    && best.is_none_or(|(used, _)| info.CurrentUsage > used)
                {
                    best = Some((info.CurrentUsage, info.Budget));
                }
            }
            best
        }
    }
}

/// Kill-switch shared with the core-side valve: `RUFFLE_AQW_NO_VRAM_VALVE`
/// also disables the pressure-driven pool squeeze here.
fn vram_valve_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("RUFFLE_AQW_NO_VRAM_VALVE").is_some())
}

/// Creates a wgpu instance with Ruffle's required configuration.
///
/// This disables indirect call validation because wgpu's validation runs a compute
/// shader that uses `array<u32>`, which requires the `DYNAMIC_ARRAY_SIZE` feature.
/// However, wgpu runs this shader without first checking if the device supports
/// that feature, causing device creation to fail on GPUs that lack it.
/// Since Ruffle doesn't use indirect draws, disabling this validation has no
/// functional impact.
///
/// See <https://github.com/gfx-rs/wgpu/issues/8799>
pub fn create_wgpu_instance(
    backends: wgpu::Backends,
    backend_options: wgpu::BackendOptions,
) -> wgpu::Instance {
    wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends,
        flags: wgpu::InstanceFlags::default()
            .difference(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL)
            .with_env(),
        backend_options,
        ..Default::default()
    })
}

/// Supersampling factor for the main render surface, from `RUFFLE_AQW_SUPERSAMPLE`
/// (default `1` = off, clamped to 1..=4). The whole scene is rendered into a surface
/// N× the swapchain resolution and linearly downsampled at present (SSAA). We inflate
/// the reported `scale_factor` by the same N, so the *logical* stage size
/// (physical / scale_factor) is unchanged — AQW keeps its NO_SCALE HUD at the real
/// 960×540 — while every vector shape and cacheAsBitmap avatar rasterizes at N×. This
/// is the only Ruffle-side lever against AQW's soft lineart (Flash renders it crisper
/// at 1×); cost is ~N² GPU memory/fill, so it stays behind the env var.
///
/// Exposed so the desktop mouse mapping can scale window coordinates by the same
/// factor (the renderer reports an N× viewport to the player).
pub fn aqw_supersample_factor() -> f32 {
    static FACTOR: OnceLock<f32> = OnceLock::new();
    *FACTOR.get_or_init(|| {
        std::env::var("RUFFLE_AQW_SUPERSAMPLE")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|n| n.is_finite())
            .map(|n| n.clamp(1.0, 4.0))
            .unwrap_or(1.0)
    })
}

/// Cap on the *supersampled* pixel area (`width × height × N²`) above which
/// SSAA drops back to 1× for that window size, re-evaluated on every resize.
/// Crispness is worth N²× fill/texture bytes at the default ~960×580 window,
/// but at fullscreen 1080p the same N²× was measured blowing an 8 GB card
/// past its VRAM budget in about a minute (WDDM paging, ~1 fps) — every
/// offscreen cache/filter/blend target scales with the render surface.
/// `RUFFLE_AQW_SUPERSAMPLE_PIXEL_CAP` overrides (in pixels; `0` = uncapped).
fn aqw_supersample_pixel_cap() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("RUFFLE_AQW_SUPERSAMPLE_PIXEL_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(2_200_000)
    })
}

/// Factor used instead when the pixel-area gate rejects the configured one.
/// `1.0` would present the window 1:1 (sharpest, but vector lineart shimmers
/// with nothing smoothing it); the default `0.8333` renders ~1600×900 for a
/// 1080p window and linearly upscales — field-tested as the preferred look:
/// the soft filtered stretch trades a little sharpness for shimmer-free
/// lineart, and its texture cost is even lower than a 1:1 fullscreen.
/// `RUFFLE_AQW_SUPERSAMPLE_FALLBACK` overrides (clamped 0.25..=1.0).
fn aqw_supersample_fallback_factor() -> f32 {
    static FACTOR: OnceLock<f32> = OnceLock::new();
    *FACTOR.get_or_init(|| {
        std::env::var("RUFFLE_AQW_SUPERSAMPLE_FALLBACK")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|n| n.is_finite())
            .map(|n| n.clamp(0.25, 1.0))
            .unwrap_or(0.8333)
    })
}

/// The SSAA factor currently in effect (post pixel-area gate), published by
/// the renderer on every viewport resize. The desktop mouse mapping reads
/// this instead of the configured factor so window↔stage coordinates always
/// follow whatever is actually rendering.
static AQW_SUPERSAMPLE_EFFECTIVE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(f32::to_bits(1.0));

pub fn aqw_current_supersample() -> f32 {
    f32::from_bits(AQW_SUPERSAMPLE_EFFECTIVE.load(std::sync::atomic::Ordering::Relaxed))
}

pub struct WgpuRenderBackend<T: RenderTarget> {
    pub(crate) descriptors: Arc<Descriptors>,
    target: T,
    surface: Surface,
    /// Configured SSAA factor; see [`aqw_supersample_factor`]. `1.0` = disabled.
    supersample: f32,
    /// Factor actually in effect for the current viewport (the configured one,
    /// or `1.0` when the pixel-area gate rejects it); see
    /// [`aqw_supersample_pixel_cap`]. Kept in sync with
    /// [`aqw_current_supersample`].
    supersample_effective: f32,
    meshes: Vec<Mesh>,
    shape_tessellator: ShapeTessellator,
    // This is currently unused - we just store it to report in
    // `get_viewport_dimensions`
    viewport_scale_factor: f64,
    texture_pool: TexturePool,
    offscreen_texture_pool: TexturePool,
    pub(crate) offscreen_buffer_pool: Arc<BufferPool<wgpu::Buffer, BufferDimensions>>,
    dynamic_transforms: DynamicTransforms,
    active_frame: ActiveFrame,
}

/// Cap on how much GPU texture memory the offscreen pool may retain. Reusing
/// offscreen targets across frames eliminates the per-frame allocation churn
/// that otherwise overwhelms the GPU driver in heavily-cached AQW rooms;
/// capping retention stops animated objects (whose bounds, and thus target
/// size, change every frame) from hoarding gigabytes of distinct-sized targets.
/// The working set of a single frame is only tens of MB, so this leaves ample
/// headroom for genuine reuse.
const OFFSCREEN_POOL_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// How many bytes of over-budget pooled offscreen textures may be freed per
/// frame. Dropping the whole pool at once (the previous behavior) dumped up to
/// 256 MB of textures into the driver's deferred-destruction queue in a single
/// frame — a visible hitch. Freeing incrementally spreads that cost out while
/// the janitor thread (see `request_device`) reclaims what's freed.
const OFFSCREEN_POOL_EVICT_BYTES_PER_FRAME: u64 = 32 * 1024 * 1024;

/// Pool retention budget while the process is near its OS GPU-memory budget:
/// hoarding idle targets is pointless once the OS is about to start demoting
/// our textures to system memory (the paging FPS collapse), so squeeze hard.
const OFFSCREEN_POOL_PRESSURE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Faster over-budget draining under pressure.
const OFFSCREEN_POOL_PRESSURE_EVICT_BYTES_PER_FRAME: u64 = 128 * 1024 * 1024;

impl WgpuRenderBackend<SwapChainTarget> {
    #[cfg(target_family = "wasm")]
    pub async fn for_canvas(
        canvas: web_sys::HtmlCanvasElement,
        webgpu: bool,
    ) -> Result<Self, Error> {
        let backends = if webgpu {
            wgpu::Backends::BROWSER_WEBGPU
        } else {
            wgpu::Backends::GL
        };
        let instance = create_wgpu_instance(
            backends,
            wgpu::BackendOptions {
                gl: wgpu::GlBackendOptions {
                    // See <https://github.com/gfx-rs/wgpu/releases/tag/v25.0.0>
                    fence_behavior: wgpu::GlFenceBehavior::AutoFinish,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let surface = instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas))?;
        let (adapter, device, queue) = request_adapter_and_device(
            backends,
            &instance,
            Some(&surface),
            wgpu::PowerPreference::HighPerformance,
        )
        .await?;
        let descriptors = Descriptors::new(instance, adapter, device, queue);
        let target =
            SwapChainTarget::new(surface, &descriptors.adapter, (1, 1), &descriptors.device);
        Self::new(Arc::new(descriptors), target)
    }

    /// # Safety
    ///  See [`wgpu::SurfaceTargetUnsafe`] variants for safety requirements.
    #[cfg(not(target_family = "wasm"))]
    pub unsafe fn for_window_unsafe(
        window: wgpu::SurfaceTargetUnsafe,
        size: (u32, u32),
        backend: wgpu::Backends,
        power_preference: wgpu::PowerPreference,
    ) -> Result<Self, Error> {
        if wgpu::Backends::SECONDARY.contains(backend) {
            tracing::warn!(
                "{} graphics backend support may not be fully supported.",
                format_list(&get_backend_names(backend), "and")
            );
        }
        let instance = create_wgpu_instance(backend, wgpu::BackendOptions::default());
        let surface = unsafe { instance.create_surface_unsafe(window)? };
        let (adapter, device, queue) = futures::executor::block_on(request_adapter_and_device(
            backend,
            &instance,
            Some(&surface),
            power_preference,
        ))?;
        let descriptors = Descriptors::new(instance, adapter, device, queue);
        let target = SwapChainTarget::new(surface, &descriptors.adapter, size, &descriptors.device);
        Self::new(Arc::new(descriptors), target)
    }

    /// # Safety
    ///  See [`wgpu::SurfaceTargetUnsafe`] variants for safety requirements.
    #[cfg(not(target_family = "wasm"))]
    pub unsafe fn recreate_surface_unsafe(
        &mut self,
        window: wgpu::SurfaceTargetUnsafe,
        size: (u32, u32),
    ) -> Result<(), Error> {
        let descriptors = &self.descriptors;
        let surface = unsafe { descriptors.wgpu_instance.create_surface_unsafe(window)? };
        self.target =
            SwapChainTarget::new(surface, &descriptors.adapter, size, &descriptors.device);
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
impl WgpuRenderBackend<crate::target::TextureTarget> {
    pub fn for_offscreen(
        size: (u32, u32),
        backend: wgpu::Backends,
        power_preference: wgpu::PowerPreference,
    ) -> Result<Self, Error> {
        if wgpu::Backends::SECONDARY.contains(backend) {
            tracing::warn!(
                "{} graphics backend support may not be fully supported.",
                format_list(&get_backend_names(backend), "and")
            );
        }
        let instance = create_wgpu_instance(backend, wgpu::BackendOptions::default());
        let (adapter, device, queue) = futures::executor::block_on(request_adapter_and_device(
            backend,
            &instance,
            None,
            power_preference,
        ))?;
        let descriptors = Descriptors::new(instance, adapter, device, queue);
        let target = crate::target::TextureTarget::new(&descriptors.device, size)?;
        Self::new(Arc::new(descriptors), target)
    }

    pub fn capture_frame(&self) -> Option<image::RgbaImage> {
        use crate::utils::buffer_to_image;
        if let Some(buffer) = &self.target.buffer {
            let (buffer, dimensions) = buffer.buffer.inner();
            Some(buffer_to_image(
                &self.descriptors.device,
                buffer,
                dimensions,
                None,
                self.target.size,
            ))
        } else {
            None
        }
    }
}

impl<T: RenderTarget> WgpuRenderBackend<T> {
    pub fn new(descriptors: Arc<Descriptors>, target: T) -> Result<Self, Error> {
        if target.width() > descriptors.limits.max_texture_dimension_2d
            || target.height() > descriptors.limits.max_texture_dimension_2d
        {
            return Err(format!(
                "Render target texture cannot be larger than {}px on either dimension (requested {} x {})",
                descriptors.limits.max_texture_dimension_2d,
                target.width(),
                target.height()
            )
                .into());
        }

        let surface = Surface::new(
            &descriptors,
            StageQuality::Low,
            target.width(),
            target.height(),
            target.format(),
        );

        let offscreen_buffer_pool = BufferPool::new(Box::new(
            |descriptors: &Descriptors, dimensions: &BufferDimensions| {
                descriptors.device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: dimensions.size(),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                })
            },
        ));

        let transforms = DynamicTransforms::new(&descriptors);
        let active_frame = ActiveFrame::new(&descriptors);

        Ok(Self {
            descriptors,
            target,
            surface,
            supersample: aqw_supersample_factor(),
            // The initial surface above is 1:1 with the target; the effective
            // factor is decided per window size in `set_viewport_dimensions`.
            supersample_effective: 1.0,
            meshes: Vec::new(),
            shape_tessellator: ShapeTessellator::new(),
            viewport_scale_factor: 1.0,
            texture_pool: TexturePool::new(),
            offscreen_texture_pool: TexturePool::new(),
            offscreen_buffer_pool: Arc::new(offscreen_buffer_pool),
            dynamic_transforms: transforms,
            active_frame,
        })
    }

    fn register_shape_internal(
        &mut self,
        shape: DistilledShape,
        bitmap_source: &dyn BitmapSource,
        scale: f32,
    ) -> Mesh {
        let shape_id = shape.id;
        let lyon_mesh =
            self.shape_tessellator
                .tessellate_shape_with_scale(shape, bitmap_source, scale);

        let mut draws = Vec::with_capacity(lyon_mesh.draws.len());
        let mut uniform_buffer = BufferBuilder::new_for_uniform(&self.descriptors.limits);
        let mut vertex_buffer = BufferBuilder::new_for_vertices(&self.descriptors.limits);
        let mut index_buffer = BufferBuilder::new_for_vertices(&self.descriptors.limits);
        let mut gradients = Vec::with_capacity(lyon_mesh.gradients.len());

        for gradient in lyon_mesh.gradients {
            gradients.push(CommonGradient::new(
                &self.descriptors,
                gradient,
                &mut uniform_buffer,
            ));
        }

        for draw in lyon_mesh.draws {
            let draw_id = draws.len();
            if let Some(draw) = PendingDraw::new(
                self,
                bitmap_source,
                draw,
                shape_id,
                draw_id,
                &mut uniform_buffer,
                &mut vertex_buffer,
                &mut index_buffer,
            ) {
                draws.push(draw);
            }
        }

        let uniform_buffer = uniform_buffer.finish(
            &self.descriptors.device,
            create_debug_label!("Shape {} uniforms", shape_id),
            wgpu::BufferUsages::UNIFORM,
        );
        let vertex_buffer = vertex_buffer.finish(
            &self.descriptors.device,
            create_debug_label!("Shape {} vertices", shape_id),
            wgpu::BufferUsages::VERTEX,
        );
        let index_buffer = index_buffer.finish(
            &self.descriptors.device,
            create_debug_label!("Shape {} indices", shape_id),
            wgpu::BufferUsages::INDEX,
        );

        let draws = draws
            .into_iter()
            .map(|d| d.finish(&self.descriptors, &uniform_buffer, &gradients))
            .collect();

        Mesh {
            draws,
            vertex_buffer,
            index_buffer,
        }
    }

    fn clamp_bitmap(&self, bitmap: &mut Bitmap) -> bool {
        let max_size = self.descriptors.limits.max_texture_dimension_2d;
        if bitmap.width() > max_size || bitmap.height() > max_size {
            let image =
                image::RgbaImage::from_raw(bitmap.width(), bitmap.height(), bitmap.data().to_vec())
                    .expect("Width and height of bitmap must match bitmap data");

            let ratio = bitmap.width() as f32 / bitmap.height() as f32;
            let mut width = bitmap.width();
            let mut height = bitmap.height();
            if width > max_size {
                width = max_size;
                height = (max_size as f32 / ratio) as u32;
            }
            if height > max_size {
                height = max_size;
                width = (max_size as f32 * ratio) as u32;
            }
            let resized = image::imageops::resize(&image, width, height, FilterType::CatmullRom);
            *bitmap = Bitmap::new(width, height, BitmapFormat::Rgba, resized.into_raw());
            true
        } else {
            false
        }
    }

    pub fn descriptors(&self) -> &Arc<Descriptors> {
        &self.descriptors
    }

    pub fn target(&self) -> &T {
        &self.target
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.descriptors.device
    }

    pub fn make_queue_sync_handle(
        &self,
        target: TextureTarget,
        index: Option<SubmissionIndex>,
        destination: BitmapHandle,
        copy_area: PixelRegion,
    ) -> Box<QueueSyncHandle> {
        match target.take_buffer() {
            None => Box::new(QueueSyncHandle::NotCopied {
                handle: destination,
                copy_area,
                descriptors: self.descriptors.clone(),
                pool: self.offscreen_buffer_pool.clone(),
            }),
            Some(TextureBufferInfo {
                buffer: MaybeOwnedBuffer::Borrowed(buffer, copy_dimensions),
                ..
            }) => Box::new(QueueSyncHandle::AlreadyCopied {
                index,
                buffer,
                copy_dimensions,
                descriptors: self.descriptors.clone(),
            }),
            Some(TextureBufferInfo {
                buffer: MaybeOwnedBuffer::Owned(..),
                ..
            }) => unreachable!("Buffer must be Borrowed as it was set to be Borrowed earlier"),
        }
    }
}

impl<T: RenderTarget + 'static> RenderBackend for WgpuRenderBackend<T> {
    fn set_viewport_dimensions(&mut self, dimensions: ViewportDimensions) {
        // Avoid panics from creating 0-sized framebuffers.
        // TODO: find a way to bubble an error when the size is too large
        let width = std::cmp::max(
            std::cmp::min(
                dimensions.width,
                self.descriptors.limits.max_texture_dimension_2d,
            ),
            1,
        );
        let height = std::cmp::max(
            std::cmp::min(
                dimensions.height,
                self.descriptors.limits.max_texture_dimension_2d,
            ),
            1,
        );
        // The swapchain always matches the real window.
        self.target.resize(&self.descriptors.device, width, height);

        // Supersampling: render into a surface `supersample`× the window and
        // linearly downsample to the swapchain at present (see submit_frame). The
        // reported scale_factor is inflated by the same factor (viewport_dimensions),
        // so the logical stage size (physical / scale_factor) is unchanged — AQW's
        // NO_SCALE HUD stays put — while shapes and cacheAsBitmap avatars rasterize
        // at N×. With supersample=1 this is identical to the old path.
        // The pixel-area gate: keep SSAA where it's cheap (small windows), fall
        // back to a sub-1× soft-stretched render where its N²× texture bytes
        // hit the VRAM wall (fullscreen); see `aqw_supersample_fallback_factor`.
        let ss = {
            let configured = self.supersample;
            let cap = aqw_supersample_pixel_cap();
            let ss_area =
                f64::from(width) * f64::from(height) * f64::from(configured).powi(2);
            if configured > 1.0 && cap != 0 && ss_area > cap as f64 {
                aqw_supersample_fallback_factor()
            } else {
                configured
            }
        };
        self.supersample_effective = ss;
        AQW_SUPERSAMPLE_EFFECTIVE.store(ss.to_bits(), std::sync::atomic::Ordering::Relaxed);
        let max = self.descriptors.limits.max_texture_dimension_2d;
        let scale_dim = |d: u32| ((d as f32 * ss).round() as u32).clamp(1, max);
        let render_width = scale_dim(width);
        let render_height = scale_dim(height);
        self.surface = Surface::new(
            &self.descriptors,
            self.surface.quality(),
            render_width,
            render_height,
            self.target.format(),
        );

        self.viewport_scale_factor = dimensions.scale_factor * f64::from(ss);
        self.texture_pool = TexturePool::new();
        // Old offscreen targets are sized for the previous viewport; drop them.
        self.offscreen_texture_pool = TexturePool::new();
    }

    fn create_context3d(
        &mut self,
        profile: Context3DProfile,
    ) -> Result<Box<dyn Context3D>, BitmapError> {
        Ok(Box::new(WgpuContext3D::new(
            self.descriptors.clone(),
            profile,
        )))
    }

    fn debug_info(&self) -> Cow<'static, str> {
        let mut result = vec![];
        result.push("Renderer: wgpu".to_string());

        let info = self.descriptors.adapter.get_info();
        result.push(format!("Adapter Backend: {:?}", info.backend));
        result.push(format!("Adapter Name: {:?}", info.name));
        result.push(format!("Adapter Device Type: {:?}", info.device_type));
        result.push(format!("Adapter Driver Name: {:?}", info.driver));
        result.push(format!("Adapter Driver Info: {:?}", info.driver_info));

        let enabled_features = self.descriptors.device.features();
        let available_features = self.descriptors.adapter.features() - enabled_features;
        let current_limits = &self.descriptors.limits;

        result.push(format!("Enabled features: {enabled_features:?}"));
        result.push(format!("Available features: {available_features:?}"));
        result.push(format!("Current limits: {current_limits:?}"));
        result.push(format!("Surface quality: {}", self.surface.quality()));
        result.push(format!("Surface samples: {}", self.surface.sample_count()));
        result.push(format!("Surface size: {:?}", self.surface.size()));

        Cow::Owned(result.join("\n"))
    }

    fn name(&self) -> &'static str {
        if cfg!(target_family = "wasm") {
            let info = self.descriptors.adapter.get_info();
            if info.backend == wgpu::Backend::BrowserWebGpu {
                "webgpu"
            } else {
                "wgpu-webgl"
            }
        } else {
            "wgpu"
        }
    }

    fn set_quality(&mut self, quality: StageQuality) {
        self.surface = Surface::new(
            &self.descriptors,
            quality,
            self.surface.size().width,
            self.surface.size().height,
            self.target.format(),
        );
    }

    fn viewport_dimensions(&self) -> ViewportDimensions {
        // Report the (supersampled) render size and inflated scale_factor so the
        // player builds commands at N× while the logical stage stays 1× (the
        // swapchain `self.target` keeps the real window size). With supersample=1
        // this is exactly the render/window size, as before.
        let size = self.surface.size();
        ViewportDimensions {
            width: size.width,
            height: size.height,
            scale_factor: self.viewport_scale_factor,
        }
    }

    #[instrument(level = "debug", skip_all)]
    fn register_shape(
        &mut self,
        shape: DistilledShape,
        bitmap_source: &dyn BitmapSource,
    ) -> ShapeHandle {
        let mesh = self.register_shape_internal(shape, bitmap_source, 1.0);
        ShapeHandle(Arc::new(mesh))
    }

    #[instrument(level = "debug", skip_all)]
    fn register_shape_with_scale(
        &mut self,
        shape: DistilledShape,
        bitmap_source: &dyn BitmapSource,
        scale: f32,
    ) -> ShapeHandle {
        let mesh = self.register_shape_internal(shape, bitmap_source, scale);
        ShapeHandle(Arc::new(mesh))
    }

    #[instrument(level = "debug", skip_all)]
    fn submit_frame(
        &mut self,
        clear: Color,
        commands: CommandList,
        cache_entries: Vec<BitmapCacheEntry>,
    ) {
        let frame_output = match self.target.get_next_texture() {
            Ok(frame) => frame,
            Err(e) => {
                tracing::warn!("Couldn't begin new render frame: {}", e);
                // Attempt to recreate the swap chain in this case.
                self.target.resize(
                    &self.descriptors.device,
                    self.target.width(),
                    self.target.height(),
                );
                return;
            }
        };

        if aqw_diagnostics_enabled() && !cache_entries.is_empty() {
            let mut total_pixels = 0_u64;
            let mut filtered_entries = 0_u32;
            let mut largest_width = 0_u32;
            let mut largest_height = 0_u32;
            let mut largest_pixels = 0_u64;

            for entry in &cache_entries {
                let texture = as_texture(&entry.handle);
                let width = texture.texture.width();
                let height = texture.texture.height();
                let pixels = u64::from(width) * u64::from(height);
                total_pixels = total_pixels.saturating_add(pixels);
                if !entry.filters.is_empty() {
                    filtered_entries = filtered_entries.saturating_add(1);
                }
                if pixels > largest_pixels {
                    largest_pixels = pixels;
                    largest_width = width;
                    largest_height = height;
                }
            }

            let approx_rgba_mb = total_pixels as f64 * 4.0 / 1_048_576.0;
            if cache_entries.len() > 4 || approx_rgba_mb > 16.0 {
                tracing::info!(
                    target: "aqw_diag",
                    cache_entries = cache_entries.len(),
                    filtered_entries,
                    total_pixels,
                    approx_rgba_mb,
                    largest_width,
                    largest_height,
                    "Bitmap cache redraws queued this frame"
                );
            }
        }

        for entry in cache_entries {
            let texture = as_texture(&entry.handle);
            let surface = Surface::new(
                &self.descriptors,
                self.surface.quality(),
                texture.texture.width(),
                texture.texture.height(),
                wgpu::TextureFormat::Rgba8Unorm,
            );
            if entry.filters.is_empty() {
                surface.draw_commands(
                    RenderTargetMode::ExistingWithColor(
                        texture.texture.clone(),
                        wgpu::Color {
                            r: f64::from(entry.clear.r) / 255.0,
                            g: f64::from(entry.clear.g) / 255.0,
                            b: f64::from(entry.clear.b) / 255.0,
                            a: f64::from(entry.clear.a) / 255.0,
                        },
                    ),
                    &self.descriptors,
                    &self.meshes,
                    entry.commands,
                    &mut self.active_frame.staging_belt,
                    &self.dynamic_transforms,
                    &mut self.active_frame.command_encoder,
                    LayerRef::None,
                    &mut self.offscreen_texture_pool,
                );
            } else {
                // We're relying on there being no impotent filters here,
                // so that we can safely start by using the actual CAB texture.
                // It's guaranteed that at least one filter would have used it and moved the target to something else,
                // letting us safely copy back to it later.
                let mut target = surface.draw_commands(
                    RenderTargetMode::ExistingWithColor(
                        texture.texture.clone(),
                        wgpu::Color {
                            r: f64::from(entry.clear.r) / 255.0,
                            g: f64::from(entry.clear.g) / 255.0,
                            b: f64::from(entry.clear.b) / 255.0,
                            a: f64::from(entry.clear.a) / 255.0,
                        },
                    ),
                    &self.descriptors,
                    &self.meshes,
                    entry.commands,
                    &mut self.active_frame.staging_belt,
                    &self.dynamic_transforms,
                    &mut self.active_frame.command_encoder,
                    LayerRef::None,
                    &mut self.offscreen_texture_pool,
                );
                for filter in entry.filters {
                    target = self.descriptors.filters.apply(
                        &self.descriptors,
                        &mut self.active_frame.command_encoder,
                        &mut self.offscreen_texture_pool,
                        &mut self.active_frame.staging_belt,
                        FilterSource::for_entire_texture(target.color_texture()),
                        filter,
                    );
                }
                run_copy_pipeline(
                    &self.descriptors,
                    texture.texture.format(),
                    &texture.texture.create_view(&Default::default()),
                    target.color_view(),
                    target.whole_frame_bind_group(&self.descriptors),
                    target.globals(),
                    target.color_texture().sample_count(),
                    false,
                    &mut self.active_frame.command_encoder,
                );
            }
            // Periodically flush GPU work to prevent OOM when many cache entries
            // accumulate (e.g. when a large container's cacheAsBitmap is skipped
            // but its hundreds of children each have their own bitmap caches).
            self.active_frame.maybe_flush(&self.descriptors);
        }

        self.surface.draw_commands_and_copy_to(
            frame_output.view(),
            // Linear filtering whenever the render surface and swapchain sizes
            // differ (downsample for SSAA, soft upscale for the sub-1× gate
            // fallback); an exact 1:1 present keeps the cheaper nearest copy.
            self.supersample_effective != 1.0,
            RenderTargetMode::FreshWithColor(wgpu::Color {
                r: f64::from(clear.r) / 255.0,
                g: f64::from(clear.g) / 255.0,
                b: f64::from(clear.b) / 255.0,
                a: f64::from(clear.a) / 255.0,
            }),
            &self.descriptors,
            &mut self.active_frame.staging_belt,
            &self.dynamic_transforms,
            &mut self.active_frame.command_encoder,
            &self.meshes,
            commands,
            LayerRef::None,
            &mut self.texture_pool,
        );
        self.active_frame.staging_belt.finish();

        self.active_frame
            .submit_for_target(&self.descriptors, &self.target, frame_output);
        // Reuse offscreen render targets across frames instead of reallocating
        // them every frame; recreating the pool every frame caused dozens of
        // large targets to be allocated per frame, ballooning driver memory to
        // many GB and OOM-crashing. Once retention exceeds the budget (distinct
        // sizes piled up, e.g. from animated objects whose target size changes
        // every frame), free idle textures incrementally instead of dropping the
        // whole pool at once, which stalled a single frame.
        let mut pool_budget = OFFSCREEN_POOL_BUDGET_BYTES;
        let mut evict_per_frame = OFFSCREEN_POOL_EVICT_BYTES_PER_FRAME;
        #[cfg(windows)]
        if !vram_valve_disabled()
            && let Some((used, budget)) = gpu_memory::query_cached()
            && budget > 0
            && used.saturating_mul(100) / budget >= 92
        {
            pool_budget = OFFSCREEN_POOL_PRESSURE_BUDGET_BYTES;
            evict_per_frame = OFFSCREEN_POOL_PRESSURE_EVICT_BYTES_PER_FRAME;
        }
        self.offscreen_texture_pool
            .evict_over_budget(pool_budget, evict_per_frame);
    }

    #[instrument(level = "debug", skip_all)]
    fn register_bitmap(&mut self, bitmap: Bitmap<'_>) -> Result<BitmapHandle, BitmapError> {
        let mut bitmap = bitmap.to_rgba();

        self.clamp_bitmap(&mut bitmap);

        let extent = wgpu::Extent3d {
            width: bitmap.width(),
            height: bitmap.height(),
            depth_or_array_layers: 1,
        };

        let texture_label = create_debug_label!("Bitmap");
        let texture = self
            .descriptors
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: texture_label.as_deref(),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC,
            });

        self.descriptors.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: Default::default(),
                aspect: wgpu::TextureAspect::All,
            },
            bitmap.data(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * extent.width),
                rows_per_image: None,
            },
            extent,
        );

        let handle = BitmapHandle(Arc::new(Texture {
            texture,
            bind_linear: Default::default(),
            bind_nearest: Default::default(),
            copy_count: Cell::new(0),
        }));

        Ok(handle)
    }

    #[instrument(level = "debug", skip_all)]
    fn update_texture(
        &mut self,
        handle: &BitmapHandle,
        bitmap: Bitmap<'_>,
        mut region: PixelRegion,
    ) -> Result<(), BitmapError> {
        if region.width() == 0 || region.height() == 0 {
            // Nothing to do. It's important to bail out now, as the
            // write_texture call panics when the source buffer is of zero size.
            return Ok(());
        }

        let texture = as_texture(handle);

        let mut bitmap = bitmap.to_rgba();
        if self.clamp_bitmap(&mut bitmap) {
            // If we're updating a resized texture, just redo the whole thing.
            // We can't trivially map pixel regions as we use a filter to resize.
            region = PixelRegion::for_whole_size(bitmap.width(), bitmap.height());
        }

        let extent = wgpu::Extent3d {
            width: region.width(),
            height: region.height(),
            depth_or_array_layers: 1,
        };

        self.active_frame.submit_direct(&self.descriptors);
        self.descriptors.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: region.x_min,
                    y: region.y_min,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &bitmap.data()[(region.y_min * texture.texture.width() * 4) as usize
                ..(region.y_max * texture.texture.width() * 4) as usize],
            wgpu::TexelCopyBufferLayout {
                offset: (region.x_min * 4) as wgpu::BufferAddress,
                bytes_per_row: Some(4 * texture.texture.width()),
                rows_per_image: None,
            },
            extent,
        );

        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    fn render_offscreen(
        &mut self,
        handle: BitmapHandle,
        commands: CommandList,
        quality: StageQuality,
        bounds: PixelRegion,
    ) -> Option<Box<dyn SyncHandle>> {
        let texture = as_texture(&handle);

        let extent = wgpu::Extent3d {
            width: texture.texture.width(),
            height: texture.texture.height(),
            depth_or_array_layers: 1,
        };

        let mut target = TextureTarget {
            size: extent,
            texture: texture.texture.clone(),
            format: wgpu::TextureFormat::Rgba8Unorm,
            buffer: None,
        };

        let frame_output = target
            .get_next_texture()
            .expect("TextureTargetFrame.get_next_texture is infallible");

        let surface = Surface::new(
            &self.descriptors,
            quality,
            texture.texture.width(),
            texture.texture.height(),
            wgpu::TextureFormat::Rgba8Unorm,
        );
        surface.draw_commands_and_copy_to(
            frame_output.view(),
            false,
            RenderTargetMode::FreshWithTexture(target.get_texture()),
            &self.descriptors,
            &mut self.active_frame.staging_belt,
            &self.dynamic_transforms,
            &mut self.active_frame.command_encoder,
            &self.meshes,
            commands,
            LayerRef::Current,
            &mut self.offscreen_texture_pool,
        );

        self.active_frame.maybe_flush(&self.descriptors);
        Some(self.make_queue_sync_handle(target, None, handle, bounds))
    }

    fn is_filter_supported(&self, filter: &Filter) -> bool {
        matches!(
            filter,
            Filter::BlurFilter(_)
                | Filter::GlowFilter(_)
                | Filter::DropShadowFilter(_)
                | Filter::ColorMatrixFilter(_)
                | Filter::ShaderFilter(_)
                | Filter::BevelFilter(_)
                | Filter::DisplacementMapFilter(_)
        )
    }

    fn is_offscreen_supported(&self) -> bool {
        true
    }

    fn gpu_memory_info(&self) -> Option<(u64, u64)> {
        #[cfg(windows)]
        {
            gpu_memory::query_cached()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn apply_filter(
        &mut self,
        source: BitmapHandle,
        source_point: (u32, u32),
        source_size: (u32, u32),
        destination: BitmapHandle,
        dest_point: (i32, i32),
        filter: Filter,
    ) -> Option<Box<dyn SyncHandle>> {
        let source_texture = as_texture(&source);
        let dest_texture = as_texture(&destination);

        let copy_area = PixelRegion::for_whole_size(
            dest_texture.texture.width(),
            dest_texture.texture.height(),
        );

        let target = TextureTarget {
            size: wgpu::Extent3d {
                width: dest_texture.texture.width(),
                height: dest_texture.texture.height(),
                depth_or_array_layers: 1,
            },
            texture: dest_texture.texture.clone(),
            format: wgpu::TextureFormat::Rgba8Unorm,
            buffer: None,
        };

        let applied_filter = self.descriptors.filters.apply(
            &self.descriptors,
            &mut self.active_frame.command_encoder,
            &mut self.offscreen_texture_pool,
            &mut self.active_frame.staging_belt,
            FilterSource {
                texture: &source_texture.texture,
                point: source_point,
                size: source_size,
            },
            filter,
        );

        let (dest_x, dest_y) = dest_point;

        let src_offset_x = dest_x.min(0).unsigned_abs();
        let src_offset_y = dest_y.min(0).unsigned_abs();

        let final_dest_x = dest_x.max(0) as u32;
        let final_dest_y = dest_y.max(0) as u32;

        let available_width = applied_filter.width().saturating_sub(src_offset_x);
        let available_height = applied_filter.height().saturating_sub(src_offset_y);
        let dest_available_width = dest_texture.texture.width().saturating_sub(final_dest_x);
        let dest_available_height = dest_texture.texture.height().saturating_sub(final_dest_y);

        let copy_width = available_width.min(dest_available_width);
        let copy_height = available_height.min(dest_available_height);

        if copy_width == 0 || copy_height == 0 {
            return None;
        }

        self.active_frame.command_encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: applied_filter.color_texture(),
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: src_offset_x,
                    y: src_offset_y,
                    z: 0,
                },
                aspect: Default::default(),
            },
            wgpu::TexelCopyTextureInfo {
                texture: &dest_texture.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: final_dest_x,
                    y: final_dest_y,
                    z: 0,
                },
                aspect: Default::default(),
            },
            wgpu::Extent3d {
                width: copy_width,
                height: copy_height,
                depth_or_array_layers: 1,
            },
        );

        self.active_frame.maybe_flush(&self.descriptors);
        Some(self.make_queue_sync_handle(target, None, destination, copy_area))
    }

    fn compile_pixelbender_shader(
        &mut self,
        shader: PixelBenderShader,
    ) -> Result<PixelBenderShaderHandle, BitmapError> {
        self.compile_pixelbender_shader_impl(shader)
    }

    fn run_pixelbender_shader(
        &mut self,
        shader: PixelBenderShaderHandle,
        arguments: &[PixelBenderShaderArgument],
        target: &PixelBenderTarget,
    ) -> Result<PixelBenderOutput, BitmapError> {
        let output_channels = shader
            .0
            .parsed_shader()
            .output_channels()
            .expect("No output parameter");
        let has_padding = output_channels == 3;

        let texture_format =
            crate::pixel_bender::temporary_texture_format_for_channels(output_channels as u32);

        let target_handle = match target {
            PixelBenderTarget::Bitmap(handle) => handle.clone(),
            PixelBenderTarget::Bytes { width, height } => {
                let extent = wgpu::Extent3d {
                    width: *width,
                    height: *height,
                    depth_or_array_layers: 1,
                };
                // FIXME - cache this texture somehow. We might also want to consider using
                // a compute shader
                let texture_label = create_debug_label!("Temporary pixelbender output texture");
                let texture = self
                    .descriptors
                    .device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: texture_label.as_deref(),
                        size: extent,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: texture_format,
                        view_formats: &[texture_format],
                        usage: wgpu::TextureUsages::TEXTURE_BINDING
                            | wgpu::TextureUsages::COPY_DST
                            | wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::COPY_SRC,
                    });
                BitmapHandle(Arc::new(Texture {
                    texture,
                    bind_linear: Default::default(),
                    bind_nearest: Default::default(),
                    copy_count: Cell::new(0),
                }))
            }
        };

        let target_texture = as_texture(&target_handle);

        let extent = wgpu::Extent3d {
            width: target_texture.texture.width(),
            height: target_texture.texture.height(),
            depth_or_array_layers: 1,
        };

        let copy_dimensions = BufferDimensions::new(
            target_texture.texture.width() as usize,
            target_texture.texture.height() as usize,
            target_texture.texture.format(),
        );
        let buffer_info = Some(TextureBufferInfo {
            buffer: MaybeOwnedBuffer::Borrowed(
                self.offscreen_buffer_pool
                    .take(&self.descriptors, copy_dimensions.clone()),
                copy_dimensions,
            ),
            copy_area: PixelRegion::for_whole_size(
                target_texture.texture.width(),
                target_texture.texture.height(),
            ),
        });

        let mut texture_target = TextureTarget {
            size: extent,
            texture: target_texture.texture.clone(),
            format: target_texture.texture.format(),
            buffer: buffer_info,
        };

        let frame_output = texture_target
            .get_next_texture()
            .expect("TextureTargetFrame.get_next_texture is infallible");

        run_pixelbender_shader_impl(
            &self.descriptors,
            shader,
            ShaderMode::ShaderJob,
            arguments,
            &target_texture.texture,
            &mut self.active_frame.command_encoder,
            Some(wgpu::RenderPassColorAttachment {
                view: frame_output.view(),
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            }),
            1,
            // When running a standalone shader, we always process the entire image
            &FilterSource::for_entire_texture(&target_texture.texture),
        )?;

        let index = Some(self.active_frame.submit_for_target(
            &self.descriptors,
            &texture_target,
            frame_output,
        ));

        let sync_handle = self.make_queue_sync_handle(
            texture_target,
            index,
            target_handle,
            PixelRegion::for_whole_size(extent.width, extent.height),
        );

        match target {
            PixelBenderTarget::Bitmap(_) => Ok(PixelBenderOutput::Bitmap(sync_handle)),
            PixelBenderTarget::Bytes { width, .. } => {
                let mut output = None;
                self.resolve_sync_handle(
                    sync_handle,
                    Box::new(|raw_pixels, buffer_width| {
                        let width = *width as usize;

                        if buffer_width as usize
                            != width * output_channels * std::mem::size_of::<f32>()
                        {
                            let mut new_pixels = Vec::new();
                            for row in raw_pixels.chunks(buffer_width as usize) {
                                let actual_row = &row[0..(width * std::mem::size_of::<[f32; 4]>())];

                                for pixel in
                                    actual_row.chunks_exact(std::mem::size_of::<[f32; 4]>())
                                {
                                    if has_padding {
                                        // Take the first three channels
                                        new_pixels.extend_from_slice(
                                            &pixel[0..(3 * std::mem::size_of::<f32>())],
                                        );
                                    } else {
                                        // Copy the pixel as-is
                                        new_pixels.extend_from_slice(pixel);
                                    }
                                }
                            }
                            output = Some(new_pixels);
                        } else {
                            output = Some(raw_pixels.to_vec());
                        };
                    }),
                )?;
                Ok(PixelBenderOutput::Bytes(output.unwrap()))
            }
        }
    }

    fn create_empty_texture(
        &mut self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<BitmapHandle, BitmapError> {
        let width = width.get();
        let height = height.get();

        if width > self.descriptors.limits.max_texture_dimension_2d
            || height > self.descriptors.limits.max_texture_dimension_2d
        {
            return Err(BitmapError::TooLarge);
        }

        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let texture_label = create_debug_label!("Bitmap");
        let texture = self
            .descriptors
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: texture_label.as_deref(),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC,
            });
        Ok(BitmapHandle(Arc::new(Texture {
            texture,
            bind_linear: Default::default(),
            bind_nearest: Default::default(),
            copy_count: Cell::new(0),
        })))
    }

    fn resolve_sync_handle(
        &mut self,
        handle: Box<dyn SyncHandle>,
        with_rgba: RgbaBufRead,
    ) -> Result<(), ruffle_render::error::Error> {
        let handle = Box::<dyn Any>::downcast::<QueueSyncHandle>(handle).unwrap();
        handle.capture(with_rgba, &mut self.active_frame);
        Ok(())
    }
}

pub async fn request_adapter_and_device(
    backend: wgpu::Backends,
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
    power_preference: wgpu::PowerPreference,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), Error> {
    let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference,
        compatible_surface: surface,
        force_fallback_adapter: false,
    }).await
        .map_err(|_e| {
            let names = get_backend_names(backend);
            if names.is_empty() {
                "Ruffle requires hardware acceleration, but no compatible graphics device was found (no backend provided?)".to_string()
            } else if cfg!(target_vendor = "apple") {
                "Ruffle does not support OpenGL on macOS/iOS.".to_string()
            } else {
                format!("Ruffle requires hardware acceleration, but no compatible graphics device was found supporting {}", format_list(&names, "or"))
            }
        })?;

    let (device, queue) = request_device(&adapter).await?;
    Ok((adapter, device, queue))
}

// We try to request the highest limits we can get away with
async fn request_device(
    adapter: &wgpu::Adapter,
) -> Result<(wgpu::Device, wgpu::Queue), wgpu::RequestDeviceError> {
    // We start off with the lowest limits we actually need - basically GL-ES 3.0
    let mut limits = wgpu::Limits::downlevel_webgl2_defaults();
    // Then we increase parts of it to the maximum supported by the adapter, to take advantage of
    // more powerful hardware or capabilities
    limits = limits.using_resolution(adapter.limits());
    limits = limits.using_alignment(adapter.limits());
    limits.max_uniform_buffer_binding_size = adapter.limits().max_uniform_buffer_binding_size;
    limits.max_inter_stage_shader_components = adapter.limits().max_inter_stage_shader_components;
    // This will be a default limit in a future wgpu version (down from 8).
    // It's required for some WebGL devices to be supported.
    limits.max_color_attachments = 4;

    let mut features = Default::default();

    let try_features = [
        wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        wgpu::Features::TEXTURE_COMPRESSION_BC,
        wgpu::Features::FLOAT32_FILTERABLE,
    ];

    for feature in try_features {
        if adapter.features().contains(feature) {
            features |= feature;
        }
    }

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: features,
            required_limits: limits,
            memory_hints: Default::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        })
        .await?;

    // By default wgpu treats every uncaptured error as fatal (a panic). An
    // out-of-memory error is recoverable enough that crashing the whole game is
    // the worst outcome: a single failed texture allocation in a crowded AQW
    // room would otherwise kill the session. Downgrade OOM to a logged warning
    // so the process survives (rendering may degrade until memory frees up),
    // while keeping validation/internal errors fatal so real bugs stay loud.
    device.on_uncaptured_error(Arc::new(handle_uncaptured_wgpu_error));

    // NOTE: a dedicated "janitor" thread looping `device.poll(Wait)` to drain
    // wgpu's deferred-destruction queue was tried here (2026-07-02) and
    // REMOVED after field testing: it didn't reduce the heavy-map RAM backlog
    // (§5) and is the prime suspect for new frame-pacing stutter and a
    // locked-up castleparty (poll contends with the render thread's submits
    // under churn). Don't re-add it.

    Ok((device, queue))
}

/// Set once the device has reported an out-of-memory error. Subsequent errors
/// are almost always cascades from the failed allocation (e.g. `create_view` on
/// a texture that never got memory), so we downgrade them to logs instead of
/// crashing — the whole point of the guard is to survive the OOM episode.
static WGPU_OOM_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Uncaptured-error handler installed on every device: logs out-of-memory (and
/// post-OOM cascade) errors instead of panicking, so the process survives;
/// validation/internal errors before any OOM stay fatal.
fn handle_uncaptured_wgpu_error(error: wgpu::Error) {
    match error {
        wgpu::Error::OutOfMemory { .. } => {
            WGPU_OOM_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                "wgpu out of memory (non-fatal): GPU memory exhausted; rendering may degrade"
            );
        }
        // Once we've seen an OOM, treat later errors as cascade fallout and keep
        // the process alive. Before any OOM, validation/internal errors are real
        // bugs and stay fatal so they're not silently hidden.
        other if WGPU_OOM_SEEN.load(std::sync::atomic::Ordering::Relaxed) => {
            tracing::error!("wgpu error after OOM (non-fatal): {other}");
        }
        other => panic!("wgpu error: {other}"),
    }
}

/// Determines how we choose our frame buffer
#[derive(Clone)]
pub enum RenderTargetMode {
    // Construct a new frame buffer, clearng it with the provided color.
    // This is used when rendering to the actual display,
    // or when applying a filter. In both cases, we have a fixed background color,
    // and don't need to blend with anything else
    FreshWithColor(wgpu::Color),
    // Construct a new frame buffer, cleared with an existing texture.
    // we will blend with the previous contents of the texture.
    // This is used in `render_offscreen`, as we need to blend with the previous
    // contents of our `BitmapData` texture
    FreshWithTexture(wgpu::Texture),
    // Use the provided texture as our frame buffer, and clear it with the given color.
    ExistingWithColor(wgpu::Texture, wgpu::Color),
}

impl RenderTargetMode {
    pub fn color(&self) -> Option<wgpu::Color> {
        match self {
            RenderTargetMode::FreshWithColor(color) => Some(*color),
            RenderTargetMode::FreshWithTexture(_) => None,
            RenderTargetMode::ExistingWithColor(_, color) => Some(*color),
        }
    }
}

pub struct ActiveFrame {
    pub staging_belt: wgpu::util::StagingBelt,
    pub command_encoder: wgpu::CommandEncoder,
    draws_since_flush: u32,
}

impl ActiveFrame {
    const MAX_DRAWS_PER_FLUSH: u32 = 100;

    pub fn new(descriptors: &Descriptors) -> Self {
        Self {
            command_encoder: descriptors
                .device
                .create_command_encoder(&Default::default()),
            staging_belt: wgpu::util::StagingBelt::new(65536),
            draws_since_flush: 0,
        }
    }

    pub fn submit_for_target<T: RenderTarget>(
        &mut self,
        descriptors: &Descriptors,
        target: &T,
        frame: T::Frame,
    ) -> SubmissionIndex {
        self.draws_since_flush = 0;
        self.staging_belt.finish();
        let draw_encoder = std::mem::replace(
            &mut self.command_encoder,
            descriptors
                .device
                .create_command_encoder(&Default::default()),
        );
        let index = target.submit(
            &descriptors.device,
            &descriptors.queue,
            Some(draw_encoder.finish()),
            frame,
        );
        self.staging_belt.recall();
        index
    }

    pub fn submit_direct(&mut self, descriptors: &Descriptors) -> SubmissionIndex {
        self.draws_since_flush = 0;
        self.staging_belt.finish();
        let draw_encoder = std::mem::replace(
            &mut self.command_encoder,
            descriptors
                .device
                .create_command_encoder(&Default::default()),
        );
        let index = descriptors.queue.submit(Some(draw_encoder.finish()));
        self.staging_belt.recall();
        index
    }

    pub fn maybe_flush(&mut self, descriptors: &Descriptors) {
        // [NA] This is kind of a hack.
        // If we do "too much" during one frame, the submission ends up being way too large and goes OutOfMemory.
        // What it is that we're OOMing on is likely buffers and temporary textures and such from render_offscreen
        // Hard to track that though... so let's just flush it out if we do more than X draws per frame
        self.draws_since_flush += 1;

        if self.draws_since_flush > Self::MAX_DRAWS_PER_FLUSH {
            self.submit_direct(descriptors);
        }
    }
}
