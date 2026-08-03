pub mod null;

use crate::bitmap::{Bitmap, BitmapHandle, BitmapSource, PixelRegion, RgbaBufRead, SyncHandle};
use crate::commands::CommandList;
use crate::error::Error;
use crate::filters::Filter;
use crate::pixel_bender::{PixelBenderShader, PixelBenderShaderHandle};
use crate::pixel_bender_support::PixelBenderShaderArgument;
use crate::quality::StageQuality;
use crate::shape_utils::DistilledShape;
use ruffle_wstr::{FromWStr, WStr};
use std::any::Any;
use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt::Debug;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use swf::{Color, Rectangle, Twips};

pub struct BitmapCacheEntry {
    pub handle: BitmapHandle,
    pub commands: CommandList,
    pub clear: Color,
    pub filters: Vec<Filter>,
}

pub trait RenderBackend: Any {
    fn viewport_dimensions(&self) -> ViewportDimensions;
    // Do not call this method directly - use `player.set_viewport_dimensions`,
    // which will ensure that the stage is properly updated as well.
    fn set_viewport_dimensions(&mut self, dimensions: ViewportDimensions);
    fn register_shape(
        &mut self,
        shape: DistilledShape,
        bitmap_source: &dyn BitmapSource,
    ) -> ShapeHandle;

    fn register_shape_with_scale(
        &mut self,
        shape: DistilledShape,
        bitmap_source: &dyn BitmapSource,
        _scale: f32,
    ) -> ShapeHandle {
        // Default implementation ignores scale
        self.register_shape(shape, bitmap_source)
    }

    fn render_offscreen(
        &mut self,
        handle: BitmapHandle,
        commands: CommandList,
        quality: StageQuality,
        bounds: PixelRegion,
    ) -> Option<Box<dyn SyncHandle>>;

    /// Applies the given filter with a `BitmapHandle` source onto a destination `BitmapHandle`.
    /// The `destination_rect` must be calculated by the caller and is assumed to be correct.
    /// Both `source_rect` and `destination_rect` must be valid (`BoundingBox::valid`).
    /// `source` may equal `destination`, in which case a temporary buffer is used internally.
    ///
    /// Returns None if the backend does not support this filter.
    fn apply_filter(
        &mut self,
        _source: BitmapHandle,
        _source_point: (u32, u32),
        _source_size: (u32, u32),
        _destination: BitmapHandle,
        _dest_point: (i32, i32),
        _filter: Filter,
    ) -> Option<Box<dyn SyncHandle>> {
        None
    }

    fn is_filter_supported(&self, _filter: &Filter) -> bool {
        false
    }

    fn is_offscreen_supported(&self) -> bool {
        false
    }

    /// Best-effort report of this process's GPU memory usage and the
    /// OS-granted budget, in bytes, when the backend has a way to measure it.
    fn gpu_memory_info(&self) -> Option<(u64, u64)> {
        None
    }

    /// `(cumulative allocations, cumulative frees, retained bytes)` of the
    /// backend's offscreen render-target pool, when it has one. The
    /// allocation delta over time measures texture churn — the driver-memory
    /// creep diagnostics watch for.
    fn offscreen_pool_stats(&self) -> Option<(u64, u64, u64)> {
        None
    }

    /// The same triple for the pool backing the *main surface* — the scene
    /// draw and the blend/mask/filter targets nested inside it.
    ///
    /// Reported separately because the two pools are managed differently, and
    /// a backend may reclaim one while letting the other accumulate; a caller
    /// watching only `offscreen_pool_stats` would see none of that.
    fn surface_pool_stats(&self) -> Option<(u64, u64, u64)> {
        None
    }

    /// The main-surface pool's biggest buckets as `(width, height, count,
    /// bytes)`, largest first — what shape the retention above actually has.
    fn surface_pool_largest(&self, _limit: usize) -> Vec<(u32, u32, usize, u64)> {
        Vec::new()
    }

    /// `(live textures, bytes)` owned by `BitmapHandle`s — bitmap caches,
    /// `BitmapData` surfaces, decoded SWF bitmaps.
    ///
    /// Deliberately outside both pool reports: these are freed when the last
    /// handle drops rather than by pool maintenance, so they are invisible to
    /// a caller adding up the pools, and they are the remainder when pool
    /// totals stay flat while process memory climbs.
    fn bitmap_texture_stats(&self) -> Option<(i64, i64)> {
        None
    }

    /// Those textures' biggest buckets as `(width, height, count, bytes)`.
    /// The totals say how much is held; only the shape says by what.
    fn bitmap_texture_largest(&self, _limit: usize) -> Vec<(u32, u32, usize, u64)> {
        Vec::new()
    }

    /// `(distinct sizes, bytes across all of them)` for those textures. How
    /// spread out the sizes are, plus a total that doubles as a check on the
    /// breakdown above being complete.
    fn bitmap_texture_buckets(&self) -> (usize, u64) {
        (0, 0)
    }

    /// Blend modes that needed a render pass of their own since the last call,
    /// as `(mode, count)` busiest-first, and cleared by reading.
    ///
    /// A backend that composites these into full-surface passes pays per pass
    /// regardless of how small the blended object is, so this says where that
    /// cost is concentrated.
    fn take_complex_blend_counts(&mut self) -> Vec<(&'static str, u64)> {
        Vec::new()
    }

    /// How much of the surface those blend passes actually covered since the
    /// last call, as `(percent, [<=1%, <=5%, <=25%, >25%] layer counts)`.
    ///
    /// The count above says how many passes ran; this says how much of each one
    /// was live. A backend that bounds its blend passes to the blended object
    /// reports the same count at a fraction of the fill.
    fn take_blend_coverage(&mut self) -> (u64, [u64; 4]) {
        (0, [0; 4])
    }

    /// Pixels allocated for blend render targets since the last call, as a
    /// percentage of what full-surface targets would have cost.
    ///
    /// Hundreds of these are alive at once in a busy scene, so their size is a
    /// memory question, not just a fill one.
    fn take_blend_alloc(&mut self) -> u64 {
        100
    }

    /// Frame-building cost since the last call, as
    /// `(encode_ms, submit_ms, frames, process_commit_mb)`.
    ///
    /// Encode is CPU spent recording commands; submit is what handing them over
    /// costs, which is where a GPU that cannot keep up shows up. Process commit
    /// is system memory, which runs out independently of VRAM.
    fn take_render_timings(&mut self) -> (u64, u64, u64, u64) {
        (0, 0, 0, 0)
    }

    fn submit_frame(
        &mut self,
        clear: swf::Color,
        commands: CommandList,
        cache_entries: Vec<BitmapCacheEntry>,
    );

    fn create_empty_texture(
        &mut self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<BitmapHandle, Error>;

    fn register_bitmap(&mut self, bitmap: Bitmap<'_>) -> Result<BitmapHandle, Error>;
    fn update_texture(
        &mut self,
        handle: &BitmapHandle,
        bitmap: Bitmap<'_>,
        region: PixelRegion,
    ) -> Result<(), Error>;

    fn create_context3d(&mut self, profile: Context3DProfile) -> Result<Box<dyn Context3D>, Error>;

    fn debug_info(&self) -> Cow<'static, str>;
    /// An internal name that is used to identify the render-backend.
    fn name(&self) -> &'static str;

    fn set_quality(&mut self, quality: StageQuality);

    fn compile_pixelbender_shader(
        &mut self,
        shader: PixelBenderShader,
    ) -> Result<PixelBenderShaderHandle, Error>;

    fn run_pixelbender_shader(
        &mut self,
        handle: PixelBenderShaderHandle,
        arguments: &[PixelBenderShaderArgument],
        target: &PixelBenderTarget,
    ) -> Result<PixelBenderOutput, Error>;

    fn resolve_sync_handle(
        &mut self,
        handle: Box<dyn SyncHandle>,
        with_rgba: RgbaBufRead,
    ) -> Result<(), Error>;
}

pub enum PixelBenderTarget {
    // The shader will write to the provided bitmap texture,
    // producing a `PixelBenderOutput::Bitmap` with the corresponding
    // `SyncHandle`
    Bitmap(BitmapHandle),
    // The shader will write to a temporary texture, which will then
    // be immediately read back as bytes (in `PixelBenderOutput::Bytes`)
    Bytes { width: u32, height: u32 },
}

pub enum PixelBenderOutput {
    Bitmap(Box<dyn SyncHandle>),
    Bytes(Vec<u8>),
}

pub trait IndexBuffer: Any {}
pub trait VertexBuffer: Any {}

pub trait ShaderModule: Any {}

pub trait Texture: Any + Debug {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

pub trait RawTexture: Any + Debug {
    fn equals(&self, other: &dyn RawTexture) -> bool;
}

#[cfg(feature = "wgpu")]
impl RawTexture for wgpu::Texture {
    fn equals(&self, other: &dyn RawTexture) -> bool {
        if let Some(other_texture) = (other as &dyn Any).downcast_ref::<wgpu::Texture>() {
            std::ptr::eq(self, other_texture)
        } else {
            false
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Context3DTextureFormat {
    Bgra,
    BgraPacked,
    BgrPacked,
    Compressed,
    CompressedAlpha,
    RgbaHalfFloat,
}

impl FromWStr for Context3DTextureFormat {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"bgra" {
            Ok(Context3DTextureFormat::Bgra)
        } else if s == b"bgraPacked4444" {
            Ok(Context3DTextureFormat::BgraPacked)
        } else if s == b"bgrPacked565" {
            Ok(Context3DTextureFormat::BgrPacked)
        } else if s == b"compressed" {
            Ok(Context3DTextureFormat::Compressed)
        } else if s == b"compressedAlpha" {
            Ok(Context3DTextureFormat::CompressedAlpha)
        } else if s == b"rgbaHalfFloat" {
            Ok(Context3DTextureFormat::RgbaHalfFloat)
        } else {
            Err(())
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Context3DBlendFactor {
    DestinationAlpha,
    DestinationColor,
    One,
    OneMinusDestinationAlpha,
    OneMinusDestinationColor,
    OneMinusSourceAlpha,
    OneMinusSourceColor,
    SourceAlpha,
    SourceColor,
    Zero,
}

impl FromWStr for Context3DBlendFactor {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"destinationAlpha" {
            Ok(Context3DBlendFactor::DestinationAlpha)
        } else if s == b"destinationColor" {
            Ok(Context3DBlendFactor::DestinationColor)
        } else if s == b"one" {
            Ok(Context3DBlendFactor::One)
        } else if s == b"oneMinusDestinationAlpha" {
            Ok(Context3DBlendFactor::OneMinusDestinationAlpha)
        } else if s == b"oneMinusDestinationColor" {
            Ok(Context3DBlendFactor::OneMinusDestinationColor)
        } else if s == b"oneMinusSourceAlpha" {
            Ok(Context3DBlendFactor::OneMinusSourceAlpha)
        } else if s == b"oneMinusSourceColor" {
            Ok(Context3DBlendFactor::OneMinusSourceColor)
        } else if s == b"sourceAlpha" {
            Ok(Context3DBlendFactor::SourceAlpha)
        } else if s == b"sourceColor" {
            Ok(Context3DBlendFactor::SourceColor)
        } else if s == b"zero" {
            Ok(Context3DBlendFactor::Zero)
        } else {
            Err(())
        }
    }
}

pub enum BufferUsage {
    DynamicDraw,
    StaticDraw,
}

pub enum ProgramType {
    Vertex,
    Fragment,
}

impl FromWStr for ProgramType {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"vertex" {
            Ok(ProgramType::Vertex)
        } else if s == b"fragment" {
            Ok(ProgramType::Fragment)
        } else {
            Err(())
        }
    }
}

pub trait Context3D: Any {
    fn profile(&self) -> Context3DProfile;
    // The BitmapHandle for the texture we're rendering to
    fn bitmap_handle(&self) -> BitmapHandle;
    // Whether or not we should actually render the texture
    // as part of stage rendering
    fn should_render(&self) -> bool;

    // Get a 'disposed' handle - this is what we store in all IndexBuffer3D
    // objects after dispose() has been called.
    fn disposed_index_buffer_handle(&self) -> Rc<dyn IndexBuffer>;

    // Get a 'disposed' handle - this is what we store in all VertexBuffer3D
    // objects after dispose() has been called.
    fn disposed_vertex_buffer_handle(&self) -> Rc<dyn VertexBuffer>;

    fn create_index_buffer(&mut self, usage: BufferUsage, num_indices: u32)
    -> Box<dyn IndexBuffer>;
    fn create_vertex_buffer(
        &mut self,
        usage: BufferUsage,
        num_vertices: u32,
        data_32_per_vertex: u8,
    ) -> Rc<dyn VertexBuffer>;

    fn create_texture(
        &mut self,
        width: u32,
        height: u32,
        format: Context3DTextureFormat,
        optimize_for_render_to_texture: bool,
        streaming_levels: u32,
    ) -> Result<Rc<dyn Texture>, Error>;
    fn create_cube_texture(
        &mut self,
        size: u32,
        format: Context3DTextureFormat,
        optimize_for_render_to_texture: bool,
        streaming_levels: u32,
    ) -> Result<Rc<dyn Texture>, Error>;

    fn upload_shaders(
        &mut self,
        module: &RefCell<Option<Rc<dyn ShaderModule>>>,
        vertex_shader_agal: Vec<u8>,
        fragment_shader_agal: Vec<u8>,
    ) -> Result<(), naga_agal::AgalError>;

    fn process_command(&mut self, command: Context3DCommand<'_>);

    fn present(&mut self);
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DVertexBufferFormat {
    Float1,
    Float2,
    Float3,
    Float4,
    Bytes4,
}

impl FromWStr for Context3DVertexBufferFormat {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"float1" {
            Ok(Context3DVertexBufferFormat::Float1)
        } else if s == b"float2" {
            Ok(Context3DVertexBufferFormat::Float2)
        } else if s == b"float3" {
            Ok(Context3DVertexBufferFormat::Float3)
        } else if s == b"float4" {
            Ok(Context3DVertexBufferFormat::Float4)
        } else if s == b"bytes4" {
            Ok(Context3DVertexBufferFormat::Bytes4)
        } else {
            Err(())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DTriangleFace {
    None,
    Back,
    Front,
    FrontAndBack,
}

impl FromWStr for Context3DTriangleFace {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"none" {
            Ok(Context3DTriangleFace::None)
        } else if s == b"back" {
            Ok(Context3DTriangleFace::Back)
        } else if s == b"front" {
            Ok(Context3DTriangleFace::Front)
        } else if s == b"frontAndBack" {
            Ok(Context3DTriangleFace::FrontAndBack)
        } else {
            Err(())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DProfile {
    Baseline,
    BaselineConstrained,
    BaselineExtended,
    Standard,
    StandardConstrained,
    StandardExtended,
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DCompareMode {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

impl FromWStr for Context3DCompareMode {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"never" {
            Ok(Context3DCompareMode::Never)
        } else if s == b"less" {
            Ok(Context3DCompareMode::Less)
        } else if s == b"equal" {
            Ok(Context3DCompareMode::Equal)
        } else if s == b"lessEqual" {
            Ok(Context3DCompareMode::LessEqual)
        } else if s == b"greater" {
            Ok(Context3DCompareMode::Greater)
        } else if s == b"notEqual" {
            Ok(Context3DCompareMode::NotEqual)
        } else if s == b"greaterEqual" {
            Ok(Context3DCompareMode::GreaterEqual)
        } else if s == b"always" {
            Ok(Context3DCompareMode::Always)
        } else {
            Err(())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DStencilAction {
    DecrementSaturate,
    DecrementWrap,
    IncrementSaturate,
    IncrementWrap,
    Invert,
    Keep,
    Set,
    Zero,
}

impl FromWStr for Context3DStencilAction {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"decrementSaturate" {
            Ok(Context3DStencilAction::DecrementSaturate)
        } else if s == b"decrementWrap" {
            Ok(Context3DStencilAction::DecrementWrap)
        } else if s == b"incrementSaturate" {
            Ok(Context3DStencilAction::IncrementSaturate)
        } else if s == b"incrementWrap" {
            Ok(Context3DStencilAction::IncrementWrap)
        } else if s == b"invert" {
            Ok(Context3DStencilAction::Invert)
        } else if s == b"keep" {
            Ok(Context3DStencilAction::Keep)
        } else if s == b"set" {
            Ok(Context3DStencilAction::Set)
        } else if s == b"zero" {
            Ok(Context3DStencilAction::Zero)
        } else {
            Err(())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DWrapMode {
    Clamp,
    ClampURepeatV,
    Repeat,
    RepeatUClampV,
}

impl FromWStr for Context3DWrapMode {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"clamp" {
            Ok(Context3DWrapMode::Clamp)
        } else if s == b"clamp_u_repeat_v" {
            Ok(Context3DWrapMode::ClampURepeatV)
        } else if s == b"repeat" {
            Ok(Context3DWrapMode::Repeat)
        } else if s == b"repeat_u_clamp_v" {
            Ok(Context3DWrapMode::RepeatUClampV)
        } else {
            Err(())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Context3DTextureFilter {
    Anisotropic16X,
    Anisotropic2X,
    Anisotropic4X,
    Anisotropic8X,
    Linear,
    Nearest,
}

impl FromWStr for Context3DTextureFilter {
    type Err = ();

    fn from_wstr(s: &WStr) -> Result<Self, Self::Err> {
        if s == b"anisotropic16x" {
            Ok(Context3DTextureFilter::Anisotropic16X)
        } else if s == b"anisotropic2x" {
            Ok(Context3DTextureFilter::Anisotropic2X)
        } else if s == b"anisotropic4x" {
            Ok(Context3DTextureFilter::Anisotropic4X)
        } else if s == b"anisotropic8x" {
            Ok(Context3DTextureFilter::Anisotropic8X)
        } else if s == b"linear" {
            Ok(Context3DTextureFilter::Linear)
        } else if s == b"nearest" {
            Ok(Context3DTextureFilter::Nearest)
        } else {
            Err(())
        }
    }
}
pub enum Context3DCommand<'a> {
    Clear {
        red: f64,
        green: f64,
        blue: f64,
        alpha: f64,
        depth: f64,
        stencil: u32,
        mask: u32,
    },
    ConfigureBackBuffer {
        width: u32,
        height: u32,
        anti_alias: u32,
        depth_and_stencil: bool,
        wants_best_resolution: bool,
        wants_best_resolution_on_browser_zoom: bool,
    },
    SetRenderToTexture {
        texture: Rc<dyn Texture>,
        enable_depth_and_stencil: bool,
        anti_alias: u32,
        surface_selector: u32,
    },
    SetRenderToBackBuffer,

    UploadToIndexBuffer {
        buffer: &'a mut dyn IndexBuffer,
        start_offset: usize,
        data: &'a [u8],
    },

    UploadToVertexBuffer {
        buffer: Rc<dyn VertexBuffer>,
        start_vertex: usize,
        data32_per_vertex: u8,
        data: &'a [u8],
    },

    DrawTriangles {
        index_buffer: &'a dyn IndexBuffer,
        first_index: usize,
        num_triangles: isize,
    },

    SetVertexBufferAt {
        index: u32,
        buffer: Option<(Rc<dyn VertexBuffer>, Context3DVertexBufferFormat)>,
        buffer_offset: u32,
    },

    SetShaders {
        module: Option<Rc<dyn ShaderModule>>,
    },
    SetProgramConstants {
        program_type: ProgramType,
        first_register: u32,
        matrix_raw_data_column_major: &'a [[u8; 4]],
    },
    SetCulling {
        face: Context3DTriangleFace,
    },
    CopyBitmapToTexture {
        source: &'a [u8],
        source_width: u32,
        source_height: u32,
        dest: Rc<dyn Texture>,
        layer: u32,
    },
    SetTextureAt {
        sampler: u32,
        texture: Option<Rc<dyn Texture>>,
        cube: bool,
    },
    SetColorMask {
        red: bool,
        green: bool,
        blue: bool,
        alpha: bool,
    },
    SetDepthTest {
        depth_mask: bool,
        pass_compare_mode: Context3DCompareMode,
    },
    SetBlendFactors {
        source_factor: Context3DBlendFactor,
        destination_factor: Context3DBlendFactor,
    },
    SetSamplerStateAt {
        sampler: u32,
        wrap: Context3DWrapMode,
        filter: Context3DTextureFilter,
    },
    SetScissorRectangle {
        rect: Option<Rectangle<Twips>>,
    },
    SetStencilActions {
        triangle_face: Context3DTriangleFace,
        compare_mode: Context3DCompareMode,
        on_both_pass: Context3DStencilAction,
        on_depth_fail: Context3DStencilAction,
        on_depth_pass_stencil_fail: Context3DStencilAction,
    },
    SetStencilReferenceValue {
        reference_value: u32,
        read_mask: u32,
        write_mask: u32,
    },
}

#[derive(Clone, Debug)]
pub struct ShapeHandle(pub Arc<dyn ShapeHandleImpl>);

pub trait ShapeHandleImpl: Any + Debug {}

#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct ViewportDimensions {
    /// The dimensions of the stage's containing viewport.
    pub width: u32,
    pub height: u32,

    /// The scale factor of the containing viewport from standard-size pixels
    /// to device-scale pixels.
    pub scale_factor: f64,
}

/// Whether the AQW CRT present filter is active. Written by the player when
/// the in-game "CRT Filter" option row (injected into AQW's Options panel)
/// is toggled, and read by the renderer each frame when choosing the final
/// present pipeline. Lives here because it must be shared between `core`
/// (which owns the toggle) and the wgpu backend (which owns the present),
/// and both already depend on this crate.
static AQW_CRT_FILTER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn aqw_crt_filter_enabled() -> bool {
    AQW_CRT_FILTER.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_aqw_crt_filter(enabled: bool) {
    AQW_CRT_FILTER.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Barrel-warp strength of the CRT present filter (`RUFFLE_AQW_CRT_WARP`
/// overrides). Shared here because the wgpu backend bakes it into the
/// present shader while the desktop host applies the SAME forward warp to
/// mouse coordinates - the screen shows content from warp(uv), so a click
/// at uv is aiming at warp(uv); using one constant keeps clicks and pixels
/// in lockstep.
///
/// Per-game default (from the launcher's env): AQW's wide 16:9 window made
/// the 0.04 curvature read oddly (field feedback), so it gets a gentler
/// 0.025; DragonFable keeps 0.04.
pub fn aqw_crt_warp_strength() -> f32 {
    use std::sync::OnceLock;
    static WARP: OnceLock<f32> = OnceLock::new();
    *WARP.get_or_init(|| {
        let is_df = std::env::var("ARTIX_RUFFLE_GAME").is_ok_and(|v| v == "df")
            || std::env::var("ARTIX_RUFFLE_GAME_ICON").is_ok_and(|v| v == "dragonfable");
        std::env::var("RUFFLE_AQW_CRT_WARP")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|n| n.is_finite())
            .map(|n| n.clamp(0.0, 0.25))
            .unwrap_or(if is_df { 0.04 } else { 0.025 })
    })
}

/// Whether the CRT present filter squeezes its (16:9) content into a centred
/// 4:3 region — the look of a widescreen signal on an old 4:3 tube, dark
/// surround on the sides. Shared for the same reason as the warp above: the
/// wgpu backend bakes it into the present shader while the desktop host must
/// apply the SAME horizontal squeeze to mouse coordinates, or clicks land off
/// the squeezed picture. Only meaningful while the CRT filter is on.
///
/// `RUFFLE_AQW_CRT_ASPECT_43` overrides (`0`/`false`/`off` = disable). Default
/// is on for AQW — the game whose 16:9 art this is for — and off for
/// DragonFable.
pub fn aqw_crt_aspect_43_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let is_df = std::env::var("ARTIX_RUFFLE_GAME").is_ok_and(|v| v == "df")
            || std::env::var("ARTIX_RUFFLE_GAME_ICON").is_ok_and(|v| v == "dragonfable");
        aqw_env_flag("RUFFLE_AQW_CRT_ASPECT_43", !is_df)
    })
}

/// Offscreen render-target pool retention, in MB, that drives the GPU-pressure
/// valve.
///
/// Measured in the field: a full room with event FX retains 118-255 MB, while
/// `castleparty` (the pathological map) retains 2458 MB — a ~10x gap, so the
/// engage/release band sits in the empty valley between the two regimes and no
/// observed scenario idles inside it.
///
/// The valve has two halves in two crates: the player clamps its cache-redraw
/// quotas from these, and the renderer squeezes its pools. They lived as
/// separate copies with a comment asking whoever edited one to remember the
/// other; they are shared from here so that cannot be got wrong.
pub const AQW_POOL_SOFT_MB: u64 = 600;
/// Retention that releases soft pressure. Below the engage threshold, so the
/// valve cannot chatter around a single value.
pub const AQW_POOL_SOFT_RELEASE_MB: u64 = 450;
/// Retention that engages hard pressure. See [`AQW_POOL_SOFT_MB`].
pub const AQW_POOL_HARD_MB: u64 = 1500;
/// Retention that drops hard pressure back to soft. See [`AQW_POOL_SOFT_MB`].
pub const AQW_POOL_HARD_RELEASE_MB: u64 = 1200;

/// Reads one of the fork's boolean environment switches.
///
/// Presence used to be the whole test in most places, which meant `NO_FOO=0`
/// *enabled* the kill switch it reads as disabling. An explicit `0`, `false`,
/// `off` or `no` (any case, surrounding whitespace ignored) turns the switch
/// off; any other value, including an empty one, turns it on. Unset returns
/// `default`.
///
/// Duplicated in `ruffle_core` rather than shared, to keep the crates
/// uncoupled for six lines of parsing; the two must agree on the spellings.
pub fn aqw_env_flag(name: &str, default: bool) -> bool {
    let Some(value) = std::env::var_os(name) else {
        return default;
    };
    let value = value.to_string_lossy();
    let value = value.trim();
    !(value == "0"
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no"))
}
