use enum_map::Enum;
use std::sync::atomic::{AtomicU64, Ordering};

use ruffle_render::{commands::RenderBlendMode, pixel_bender::PixelBenderShaderHandle};
use swf::BlendMode;

#[derive(Enum, Debug, Copy, Clone)]
pub enum ComplexBlend {
    Multiply,   // Can't be trivial, 0 alpha is special case
    Lighten,    // Might be trivial but I can't reproduce the right colors
    Darken,     // Might be trivial but I can't reproduce the right colors
    Difference, // Can't be trivial, relies on abs operation
    Invert,     // May be trivial using a constant? Hard because it's without premultiplied alpha
    Alpha,      // Can't be trivial, requires layer tracking
    Erase,      // Can't be trivial, requires layer tracking
    Overlay,    // Can't be trivial, big math expression
    HardLight,  // Can't be trivial, big math expression
}

static COMPLEX_BLEND_COUNTS: [AtomicU64; 10] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn complex_blend_index(blend: ComplexBlend) -> usize {
    match blend {
        ComplexBlend::Multiply => 0,
        ComplexBlend::Lighten => 1,
        ComplexBlend::Darken => 2,
        ComplexBlend::Difference => 3,
        ComplexBlend::Invert => 4,
        ComplexBlend::Alpha => 5,
        ComplexBlend::Erase => 6,
        ComplexBlend::Overlay => 7,
        ComplexBlend::HardLight => 8,
    }
}

pub fn note_complex_blend(blend: ComplexBlend) {
    COMPLEX_BLEND_COUNTS[complex_blend_index(blend)].fetch_add(1, Ordering::Relaxed);
}

pub fn note_shader_blend() {
    COMPLEX_BLEND_COUNTS[9].fetch_add(1, Ordering::Relaxed);
}

static BLEND_MULTIPLY_TOTAL: AtomicU64 = AtomicU64::new(0);
static BLEND_MULTIPLY_ON_OPAQUE: AtomicU64 = AtomicU64::new(0);
static BLEND_MULTIPLY_OPAQUE_PX: AtomicU64 = AtomicU64::new(0);

pub fn note_multiply_dest(is_multiply: bool, dest_opaque: bool, alloc_px: u64) {
    if !is_multiply {
        return;
    }
    BLEND_MULTIPLY_TOTAL.fetch_add(1, Ordering::Relaxed);
    if dest_opaque {
        BLEND_MULTIPLY_ON_OPAQUE.fetch_add(1, Ordering::Relaxed);
        BLEND_MULTIPLY_OPAQUE_PX.fetch_add(alloc_px, Ordering::Relaxed);
    }
}

pub fn take_blend_dest_opacity() -> (u64, u64, u64) {
    let opaque = BLEND_MULTIPLY_ON_OPAQUE.swap(0, Ordering::Relaxed);
    let total = BLEND_MULTIPLY_TOTAL.swap(0, Ordering::Relaxed);
    let px = BLEND_MULTIPLY_OPAQUE_PX.swap(0, Ordering::Relaxed);
    (opaque, total, px / (1024 * 1024))
}

static BLEND_COVERED_PX: AtomicU64 = AtomicU64::new(0);
static BLEND_SURFACE_PX: AtomicU64 = AtomicU64::new(0);
static BLEND_COVER_HIST: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static BLEND_ALLOC_PX: AtomicU64 = AtomicU64::new(0);
static BLEND_ALLOC_FULL_PX: AtomicU64 = AtomicU64::new(0);

static TRIVIAL_BLEND_COUNTS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static TRIVIAL_LIVE_PX: AtomicU64 = AtomicU64::new(0);
static TRIVIAL_TARGET_PX: AtomicU64 = AtomicU64::new(0);
static TRIVIAL_FULL_PX: AtomicU64 = AtomicU64::new(0);
static TRIVIAL_COVER_HIST: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

pub fn note_trivial_blend(mode: Option<BlendMode>) {
    let index = match mode {
        Some(BlendMode::Layer) => 0,
        Some(BlendMode::Screen) => 1,
        Some(BlendMode::Add) => 2,
        Some(BlendMode::Subtract) => 3,
        Some(BlendMode::Multiply) => 4,
        _ => 5,
    };
    TRIVIAL_BLEND_COUNTS[index].fetch_add(1, Ordering::Relaxed);
}

pub fn note_trivial_blend_target(live_px: u64, target_px: u64, full_px: u64) {
    TRIVIAL_LIVE_PX.fetch_add(live_px, Ordering::Relaxed);
    TRIVIAL_TARGET_PX.fetch_add(target_px, Ordering::Relaxed);
    TRIVIAL_FULL_PX.fetch_add(full_px, Ordering::Relaxed);

    let permille = live_px
        .saturating_mul(1000)
        .checked_div(target_px)
        .unwrap_or(0);
    let bucket = match permille {
        0..=10 => 0,
        11..=50 => 1,
        51..=250 => 2,
        _ => 3,
    };
    TRIVIAL_COVER_HIST[bucket].fetch_add(1, Ordering::Relaxed);
}

pub fn take_trivial_blend_target() -> (u64, u64, u64, [u64; 4]) {
    let live = TRIVIAL_LIVE_PX.swap(0, Ordering::Relaxed);
    let target = TRIVIAL_TARGET_PX.swap(0, Ordering::Relaxed);
    let full = TRIVIAL_FULL_PX.swap(0, Ordering::Relaxed);
    let cover = live.saturating_mul(100).checked_div(target).unwrap_or(0);
    let alloc = target.saturating_mul(100).checked_div(full).unwrap_or(0);
    let hist = std::array::from_fn(|i| TRIVIAL_COVER_HIST[i].swap(0, Ordering::Relaxed));
    (cover, target / (1024 * 1024), alloc, hist)
}

pub fn take_trivial_blend_counts() -> Vec<(&'static str, u64)> {
    const NAMES: [&str; 6] = ["layer", "screen", "add", "subtract", "multiply", "normal"];
    let mut counts: Vec<(&'static str, u64)> = NAMES
        .iter()
        .zip(TRIVIAL_BLEND_COUNTS.iter())
        .filter_map(|(name, slot)| {
            let count = slot.swap(0, Ordering::Relaxed);
            (count > 0).then_some((*name, count))
        })
        .collect();
    counts.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts
}

pub fn note_blend_alloc(alloc_px: u64, surface_px: u64) {
    BLEND_ALLOC_PX.fetch_add(alloc_px, Ordering::Relaxed);
    BLEND_ALLOC_FULL_PX.fetch_add(surface_px, Ordering::Relaxed);
}

pub fn take_blend_alloc() -> u64 {
    let alloc = BLEND_ALLOC_PX.swap(0, Ordering::Relaxed);
    let full = BLEND_ALLOC_FULL_PX.swap(0, Ordering::Relaxed);
    alloc.saturating_mul(100).checked_div(full).unwrap_or(0)
}

pub fn note_blend_coverage(covered_px: u64, surface_px: u64) {
    BLEND_COVERED_PX.fetch_add(covered_px, Ordering::Relaxed);
    BLEND_SURFACE_PX.fetch_add(surface_px, Ordering::Relaxed);

    let permille = covered_px
        .saturating_mul(1000)
        .checked_div(surface_px)
        .unwrap_or(0);
    let bucket = match permille {
        0..=10 => 0,
        11..=50 => 1,
        51..=250 => 2,
        _ => 3,
    };
    BLEND_COVER_HIST[bucket].fetch_add(1, Ordering::Relaxed);
}

pub fn take_blend_coverage() -> (u64, [u64; 4]) {
    let covered = BLEND_COVERED_PX.swap(0, Ordering::Relaxed);
    let surface = BLEND_SURFACE_PX.swap(0, Ordering::Relaxed);
    let percent = covered
        .saturating_mul(100)
        .checked_div(surface)
        .unwrap_or(0);
    let hist = std::array::from_fn(|i| BLEND_COVER_HIST[i].swap(0, Ordering::Relaxed));
    (percent, hist)
}

pub fn take_complex_blend_counts() -> Vec<(&'static str, u64)> {
    const NAMES: [&str; 10] = [
        "multiply",
        "lighten",
        "darken",
        "difference",
        "invert",
        "alpha",
        "erase",
        "overlay",
        "hardlight",
        "pixelbender",
    ];
    let mut counts: Vec<(&'static str, u64)> = NAMES
        .iter()
        .zip(COMPLEX_BLEND_COUNTS.iter())
        .filter_map(|(name, slot)| {
            let count = slot.swap(0, Ordering::Relaxed);
            (count > 0).then_some((*name, count))
        })
        .collect();
    counts.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts
}

#[derive(Debug, Clone)]
pub enum BlendType {
    /// Trivial blends can be expressed with just a "draw bitmap" with blend states
    Trivial(TrivialBlend),

    /// Complex blends require a shader to express, so they are separated out into their own render
    Complex(ComplexBlend),

    /// Invoke a custom `PixelBender` shader.
    Shader(PixelBenderShaderHandle),
}

impl BlendType {
    pub fn from(mode: RenderBlendMode) -> BlendType {
        match mode {
            RenderBlendMode::Builtin(BlendMode::Normal) => BlendType::Trivial(TrivialBlend::Normal),
            RenderBlendMode::Builtin(BlendMode::Layer) => BlendType::Trivial(TrivialBlend::Normal),
            RenderBlendMode::Builtin(BlendMode::Multiply) => {
                BlendType::Complex(ComplexBlend::Multiply)
            }
            RenderBlendMode::Builtin(BlendMode::Screen) => BlendType::Trivial(TrivialBlend::Screen),
            RenderBlendMode::Builtin(BlendMode::Lighten) => {
                BlendType::Complex(ComplexBlend::Lighten)
            }
            RenderBlendMode::Builtin(BlendMode::Darken) => BlendType::Complex(ComplexBlend::Darken),
            RenderBlendMode::Builtin(BlendMode::Difference) => {
                BlendType::Complex(ComplexBlend::Difference)
            }
            RenderBlendMode::Builtin(BlendMode::Add) => BlendType::Trivial(TrivialBlend::Add),
            RenderBlendMode::Builtin(BlendMode::Subtract) => {
                BlendType::Trivial(TrivialBlend::Subtract)
            }
            RenderBlendMode::Builtin(BlendMode::Invert) => BlendType::Complex(ComplexBlend::Invert),
            RenderBlendMode::Builtin(BlendMode::Alpha) => BlendType::Complex(ComplexBlend::Alpha),
            RenderBlendMode::Builtin(BlendMode::Erase) => BlendType::Complex(ComplexBlend::Erase),
            RenderBlendMode::Builtin(BlendMode::Overlay) => {
                BlendType::Complex(ComplexBlend::Overlay)
            }
            RenderBlendMode::Builtin(BlendMode::HardLight) => {
                BlendType::Complex(ComplexBlend::HardLight)
            }
            RenderBlendMode::Shader(shader) => BlendType::Shader(shader),
        }
    }

    pub fn default_color(&self) -> wgpu::Color {
        wgpu::Color::TRANSPARENT
    }
}

#[derive(Enum, Debug, Copy, Clone)]
pub enum TrivialBlend {
    Normal,
    Add,
    Subtract,
    Screen,
    Multiply,
}

impl TrivialBlend {
    pub fn blend_state(self) -> wgpu::BlendState {
        // out = <src_factor> * src <operation> <dst_factor> * dst
        match self {
            TrivialBlend::Normal => wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            TrivialBlend::Add => wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
            TrivialBlend::Screen => wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrc,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
            TrivialBlend::Subtract => wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::ReverseSubtract,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
            TrivialBlend::Multiply => wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::Dst,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
        }
    }
}
