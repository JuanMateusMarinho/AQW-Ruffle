// Remove this when we decide on how to handle multithreaded rendering (especially on wasm)
#![allow(clippy::arc_with_non_send_sync)]

use crate::backend::ActiveFrame;
use crate::bitmaps::BitmapSamplers;
use crate::buffer_pool::{BufferPool, PoolEntry};
use crate::descriptors::Quad;
use crate::mesh::BitmapBinds;
use crate::pipelines::Pipelines;
use crate::target::{RenderTarget, SwapChainTarget};
use crate::utils::{
    BufferDimensions, capture_image, create_buffer_with_data, format_list, get_backend_names,
};
use bytemuck::{Pod, Zeroable};
use descriptors::Descriptors;
use enum_map::Enum;
use ruffle_render::backend::RawTexture;
use ruffle_render::bitmap::{BitmapHandle, BitmapHandleImpl, PixelRegion, SyncHandle};
use ruffle_render::shape_utils::GradientType;
use ruffle_render::tessellator::{Gradient as TessGradient, Vertex as TessVertex};
use std::any::Any;
use std::cell::{Cell, OnceCell};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use swf::GradientSpread;
pub use wgpu;

type Error = Box<dyn std::error::Error>;

#[macro_use]
pub mod utils;

mod bitmaps;
mod context3d;
mod globals;
mod pipelines;
mod pixel_bender;
pub mod target;

pub mod backend;
mod blend;
mod buffer_builder;
mod buffer_pool;
mod content_bounds;
#[cfg(feature = "clap")]
pub mod clap;
pub mod descriptors;
mod dynamic_transforms;
mod filters;
mod layouts;
mod mesh;
mod shaders;
mod surface;

impl BitmapHandleImpl for Texture {}

pub fn as_texture(handle: &BitmapHandle) -> &Texture {
    <dyn Any>::downcast_ref(&*handle.0).unwrap()
}

pub fn raw_texture_as_texture(handle: &dyn RawTexture) -> &wgpu::Texture {
    <dyn Any>::downcast_ref(handle).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum MaskState {
    NoMask,
    DrawMaskStencil,
    DrawMaskedContent,
    ClearMaskStencil,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Transforms {
    world_matrix: [[f32; 4]; 4],
    mult_color: [f32; 4],
    add_color: [f32; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct TextureTransforms {
    u_matrix: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct PosVertex {
    position: [f32; 2],
}

impl From<TessVertex> for PosVertex {
    fn from(vertex: TessVertex) -> Self {
        Self {
            position: [vertex.x, vertex.y],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct PosColorVertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl From<TessVertex> for PosColorVertex {
    fn from(vertex: TessVertex) -> Self {
        Self {
            position: [vertex.x, vertex.y],
            color: [
                f32::from(vertex.color.r) / 255.0,
                f32::from(vertex.color.g) / 255.0,
                f32::from(vertex.color.b) / 255.0,
                f32::from(vertex.color.a) / 255.0,
            ],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct GradientUniforms {
    focal_point: f32,
    interpolation: i32,
    shape: i32,
    repeat: i32,
}

impl From<TessGradient> for GradientUniforms {
    fn from(gradient: TessGradient) -> Self {
        Self {
            focal_point: gradient.focal_point.to_f32().clamp(-0.98, 0.98),
            interpolation: (gradient.interpolation == swf::GradientInterpolation::LinearRgb) as i32,
            shape: match gradient.gradient_type {
                GradientType::Linear => 1,
                GradientType::Radial => 2,
                GradientType::Focal => 3,
            },
            repeat: match gradient.repeat_mode {
                GradientSpread::Pad => 1,
                GradientSpread::Reflect => 2,
                GradientSpread::Repeat => 3,
            },
        }
    }
}

#[derive(Debug)]
pub enum QueueSyncHandle {
    AlreadyCopied {
        index: Option<wgpu::SubmissionIndex>,
        buffer: PoolEntry<wgpu::Buffer, BufferDimensions>,
        copy_dimensions: BufferDimensions,
        descriptors: Arc<Descriptors>,
    },
    NotCopied {
        handle: BitmapHandle,
        copy_area: PixelRegion,
        descriptors: Arc<Descriptors>,
        pool: Arc<BufferPool<wgpu::Buffer, BufferDimensions>>,
    },
}

impl SyncHandle for QueueSyncHandle {}

impl QueueSyncHandle {
    pub fn capture<R, F: FnOnce(&[u8], u32) -> R>(
        self,
        with_rgba: F,
        frame: &mut ActiveFrame,
    ) -> R {
        match self {
            QueueSyncHandle::AlreadyCopied {
                index,
                buffer,
                copy_dimensions,
                descriptors,
            } => capture_image(
                &descriptors.device,
                &buffer,
                &copy_dimensions,
                index,
                with_rgba,
            ),
            QueueSyncHandle::NotCopied {
                handle,
                copy_area,
                descriptors,
                pool,
            } => {
                let texture = as_texture(&handle);

                let buffer_dimensions = BufferDimensions::new(
                    copy_area.width() as usize,
                    copy_area.height() as usize,
                    texture.texture.format(),
                );

                let buffer = pool.take(&descriptors, buffer_dimensions.clone());
                frame.command_encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: copy_area.x_min,
                            y: copy_area.y_min,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(buffer_dimensions.padded_bytes_per_row),
                            rows_per_image: None,
                        },
                    },
                    wgpu::Extent3d {
                        width: copy_area.width(),
                        height: copy_area.height(),
                        depth_or_array_layers: 1,
                    },
                );
                let index = frame.submit_direct(&descriptors);

                let image = capture_image(
                    &descriptors.device,
                    &buffer,
                    &buffer_dimensions,
                    Some(index),
                    with_rgba,
                );

                // After we've read pixels from a texture enough times, we'll store this buffer so that
                // future reads will be faster (it'll copy as part of the draw process instead)
                texture
                    .copy_count
                    .set(texture.copy_count.get().saturating_add(1));

                image
            }
        }
    }
}

/// Live GPU memory behind `BitmapHandle`s: bitmap caches, the `BitmapData`
/// surfaces content draws into, and decoded SWF bitmaps.
///
/// Neither texture pool can see any of it. These textures belong to the
/// handle and are freed only when its last clone drops, so a diagnostic that
/// adds up the two pools can report a healthy total while this grows without
/// bound — which is exactly the shape of the unaccounted commit growth.
static BITMAP_TEXTURE_BYTES: AtomicI64 = AtomicI64::new(0);
static BITMAP_TEXTURE_COUNT: AtomicI64 = AtomicI64::new(0);

/// `(live textures, bytes)` currently held by `BitmapHandle`s.
pub fn bitmap_texture_stats() -> (i64, i64) {
    (
        BITMAP_TEXTURE_COUNT.load(Ordering::Relaxed),
        BITMAP_TEXTURE_BYTES.load(Ordering::Relaxed),
    )
}

/// Live bitmap textures grouped by dimensions, tracked only under the AQW
/// diagnostics flag because it costs a lock on every texture birth and death.
///
/// The totals alone cannot say what is accumulating: a gigabyte is thousands
/// of avatar-sized caches or a few hundred room-sized bitmaps, and those have
/// nothing to do with each other. The shape names the source.
fn bitmap_texture_sizes()
-> &'static std::sync::Mutex<std::collections::HashMap<(u32, u32), (usize, u64)>> {
    static SIZES: OnceLock<
        std::sync::Mutex<std::collections::HashMap<(u32, u32), (usize, u64)>>,
    > = OnceLock::new();
    SIZES.get_or_init(Default::default)
}

fn track_bitmap_texture(texture: &wgpu::Texture, bytes: i64, born: bool) {
    if !crate::backend::aqw_diagnostics_enabled() {
        return;
    }
    let key = (texture.width(), texture.height());
    let Ok(mut sizes) = bitmap_texture_sizes().lock() else {
        return;
    };
    let entry = sizes.entry(key).or_insert((0, 0));
    if born {
        entry.0 += 1;
        entry.1 += bytes.max(0) as u64;
    } else {
        entry.0 = entry.0.saturating_sub(1);
        entry.1 = entry.1.saturating_sub(bytes.max(0) as u64);
        if entry.0 == 0 {
            sizes.remove(&key);
        }
    }
}

/// `(distinct sizes, bytes across all of them)`.
///
/// The pair the largest-buckets list cannot provide: a handful of sizes means
/// a few repeated allocations, hundreds means the same content re-rasterized
/// at dimensions that keep shifting. The byte total is also the control — if
/// it does not come close to the reported live total, the tracking is wrong
/// and the shape below it means nothing.
pub fn bitmap_texture_buckets() -> (usize, u64) {
    let Ok(sizes) = bitmap_texture_sizes().lock() else {
        return (0, 0);
    };
    (sizes.len(), sizes.values().map(|(_, bytes)| bytes).sum())
}

/// The biggest live bitmap-texture buckets as `(width, height, count, bytes)`.
pub fn bitmap_texture_largest(limit: usize) -> Vec<(u32, u32, usize, u64)> {
    let Ok(sizes) = bitmap_texture_sizes().lock() else {
        return Vec::new();
    };
    let mut buckets: Vec<_> = sizes
        .iter()
        .map(|((w, h), (count, bytes))| (*w, *h, *count, *bytes))
        .collect();
    buckets.sort_by(|a, b| b.3.cmp(&a.3));
    buckets.truncate(limit);
    buckets
}

fn texture_bytes(texture: &wgpu::Texture) -> i64 {
    let size = texture.size();
    let block = texture.format().block_copy_size(None).unwrap_or(4);
    i64::from(size.width)
        * i64::from(size.height)
        * i64::from(size.depth_or_array_layers)
        * i64::from(block)
}

#[derive(Debug)]
pub struct Texture {
    pub(crate) texture: wgpu::Texture,
    bind_linear: OnceCell<BitmapBinds>,
    bind_nearest: OnceCell<BitmapBinds>,
    copy_count: Cell<u8>,
}

impl Drop for Texture {
    fn drop(&mut self) {
        let bytes = texture_bytes(&self.texture);
        BITMAP_TEXTURE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        BITMAP_TEXTURE_COUNT.fetch_sub(1, Ordering::Relaxed);
        track_bitmap_texture(&self.texture, bytes, false);
    }
}

impl Texture {
    /// The only way to build one, so the accounting above cannot be bypassed
    /// by a new call site.
    pub(crate) fn new(texture: wgpu::Texture) -> Self {
        let bytes = texture_bytes(&texture);
        BITMAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
        BITMAP_TEXTURE_COUNT.fetch_add(1, Ordering::Relaxed);
        track_bitmap_texture(&texture, bytes, true);
        Self {
            texture,
            bind_linear: Default::default(),
            bind_nearest: Default::default(),
            copy_count: Cell::new(0),
        }
    }

    pub fn bind_group(
        &self,
        smoothed: bool,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        quad: &Quad,
        handle: BitmapHandle,
        samplers: &BitmapSamplers,
    ) -> &BitmapBinds {
        let bind = match smoothed {
            true => &self.bind_linear,
            false => &self.bind_nearest,
        };
        bind.get_or_init(|| {
            BitmapBinds::new(
                device,
                layout,
                samplers.get_sampler(false, smoothed),
                &quad.texture_transforms,
                0 as wgpu::BufferAddress,
                self.texture.create_view(&Default::default()),
                create_debug_label!("Bitmap {:?} bind group (smoothed: {})", handle.0, smoothed),
            )
        })
    }
}
