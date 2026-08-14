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

/// Per-mode tally of the blend chunks that had to be split into their own
/// render pass, since the last [`take_complex_blend_counts`].
///
/// Every one of these allocates a target the size of the whole surface and
/// composites across it, whatever area the object actually covers — measured
/// at ~145 per frame in a crowded room, which is where the frame time goes.
/// Flash rasterized the same blends on the CPU over the object's bounds only,
/// so its cost scaled with the object, not the screen; that difference is why
/// two players in particular gear can cost more than a room full of plain
/// ones.
///
/// Which modes dominate decides the fix: `Multiply`/`Lighten`/`Darken` are
/// candidates for GPU blend state with no intermediate target at all (see the
/// notes on [`ComplexBlend`]), while `Alpha`/`Erase` need layer tracking and
/// leave no cheap way out.
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

/// PixelBender blends share the same full-surface cost; slot 9 keeps them
/// visible without implying they have the same fix.
pub fn note_shader_blend() {
    COMPLEX_BLEND_COUNTS[9].fetch_add(1, Ordering::Relaxed);
}

/// Sizes the one complex mode that fixed-function blend state can express
/// exactly: multiply, and only where the destination is opaque.
///
/// `DST_COLOR` / `ONE_MINUS_SRC_ALPHA` yields `src*dst + dst*(1-src.a)`. The
/// term it omits, `src*(1-dst.a)`, vanishes at `dst.a == 1` -- and over a
/// transparent destination it is what makes the art disappear, which is the
/// reason upstream abandoned this and the reason it is worth re-asking rather
/// than assuming. A destination is transparent only inside another blend or
/// filter target, cleared that way deliberately; against the scene it carries
/// the stage colour.
///
/// Counting only. Whether the fold is worth building is exactly the ratio this
/// reports, and the population is 55% of complex blends in a crowded room.
///
/// One caveat to read it with: an Alpha or Erase drawn into the same target
/// could punch its opacity back out, and this does not track that. Those two
/// are measured at zero in AQW, and `blend_modes` reports them alongside, so a
/// run where they appear is a run where this reads as an upper bound.
static BLEND_MULTIPLY_TOTAL: AtomicU64 = AtomicU64::new(0);
static BLEND_MULTIPLY_ON_OPAQUE: AtomicU64 = AtomicU64::new(0);
static BLEND_MULTIPLY_OPAQUE_PX: AtomicU64 = AtomicU64::new(0);

/// Takes plain booleans rather than a [`ComplexBlend`] because it has to be
/// called after the fold has possibly moved this blend into the trivial path,
/// where the complex mode no longer exists. Reading identically in both arms is
/// the point: a counter that empties when the lever is on would be comparing a
/// measurement against its own absence.
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

/// Drains the multiply tally as `(onto_opaque, total, opaque_megapixels)`.
///
/// The megapixels are the absolute scale the count cannot carry: a thousand
/// passes over 40x40 targets and a hundred over full-screen ones are the same
/// ratio and nowhere near the same prize.
pub fn take_blend_dest_opacity() -> (u64, u64, u64) {
    let opaque = BLEND_MULTIPLY_ON_OPAQUE.swap(0, Ordering::Relaxed);
    let total = BLEND_MULTIPLY_TOTAL.swap(0, Ordering::Relaxed);
    let px = BLEND_MULTIPLY_OPAQUE_PX.swap(0, Ordering::Relaxed);
    (opaque, total, px / (1024 * 1024))
}

/// How much of the surface complex blend passes actually cover, since the last
/// [`take_blend_coverage`]: summed content area, summed full-surface area, and
/// a histogram of per-layer coverage in the buckets <=1%, <=5%, <=25%, >25%.
///
/// This is the number that decides whether bounding those passes is worth
/// anything. The layer *count* cannot answer it -- scissoring removes no
/// passes, it shrinks them -- so a drop in [`take_complex_blend_counts`] would
/// mean blends went missing, not that the fill got cheaper.
static BLEND_COVERED_PX: AtomicU64 = AtomicU64::new(0);
static BLEND_SURFACE_PX: AtomicU64 = AtomicU64::new(0);
static BLEND_COVER_HIST: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Pixels actually allocated for complex blend targets, against what a
/// full-surface target would have cost. This is the memory side: a crowded
/// room keeps hundreds of these alive at once, and at full surface size that
/// alone pushed VRAM past the OS grant.
static BLEND_ALLOC_PX: AtomicU64 = AtomicU64::new(0);
static BLEND_ALLOC_FULL_PX: AtomicU64 = AtomicU64::new(0);

/// Per-mode tally of the blends that fold into GPU blend state, since the last
/// [`take_trivial_blend_counts`].
///
/// These were the population nothing was measuring. Being expressible as blend
/// state only saves them the compositing *pass* -- everything before it is
/// identical to the complex case: a render target, a clear of it, the subtree
/// rendered into it, and a quad to draw it back. Until this was measured that
/// target was always the size of the whole surface, since both the shrink and
/// the scissor were gated on `is_complex`, and a 30x30 glow with `Add` cost the
/// same as one covering the screen. Counted at the point of emission, where a
/// subtree that drew nothing has already been dropped.
///
/// `Add` dominates, and it is worth knowing why the obvious guess is wrong:
/// this looks like it should be `Layer`, but the core only emits a blend when
/// the mode is not Normal, and AQW's trivial blends are glow -- FX from the
/// shared asset file and the map's own NPCs. Measured over a session: 66551
/// `Add`, 1692 `Screen`, 23 `Layer`. A plain `Normal` can only come from the
/// shader-in-mask fallback or from `RUFFLE_AQW_BLEND_AS_NORMAL`, so reading
/// non-zero on it outside those two means something else demoted a blend.
static TRIVIAL_BLEND_COUNTS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// How much of the trivial blend targets is live content, what they were
/// allocated at, and what full-surface ones would have cost, since the last
/// [`take_trivial_blend_target`].
///
/// Coverage answers the same question `blend_cover` does for complex blends,
/// with the difference that a trivial target pays for its size three times
/// over -- alloc, clear, composite draw -- rather than in the pass alone. It
/// read 0-1% for the whole of the measurement that motivated bounding these,
/// so it should now read high: the target is the content. The allocation
/// percentage is the mirror of `blend_alloc`, and the pixel total is the
/// absolute scale, without which a small percentage over a large population
/// reads as harmless.
static TRIVIAL_LIVE_PX: AtomicU64 = AtomicU64::new(0);
static TRIVIAL_TARGET_PX: AtomicU64 = AtomicU64::new(0);
static TRIVIAL_FULL_PX: AtomicU64 = AtomicU64::new(0);
static TRIVIAL_COVER_HIST: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Records a trivial blend under the mode the content asked for, which
/// [`TrivialBlend`] cannot answer: `Normal` and `Layer` share a variant there
/// and only one of them is a real population.
/// `Multiply` has a slot of its own because it only reaches here once the
/// fixed-function fold has claimed it, and lumping it under `normal` would
/// hide exactly the number the fold has to be judged by.
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

/// Drains the trivial target tally as
/// `(live_percent, megapixels, alloc_percent, [hist; 4])`.
pub fn take_trivial_blend_target() -> (u64, u64, u64, [u64; 4]) {
    let live = TRIVIAL_LIVE_PX.swap(0, Ordering::Relaxed);
    let target = TRIVIAL_TARGET_PX.swap(0, Ordering::Relaxed);
    let full = TRIVIAL_FULL_PX.swap(0, Ordering::Relaxed);
    let cover = live.saturating_mul(100).checked_div(target).unwrap_or(0);
    let alloc = target.saturating_mul(100).checked_div(full).unwrap_or(0);
    let hist = std::array::from_fn(|i| TRIVIAL_COVER_HIST[i].swap(0, Ordering::Relaxed));
    (cover, target / (1024 * 1024), alloc, hist)
}

/// Drains the trivial tally into `mode=count` pairs, busiest first, omitting
/// zeros.
pub fn take_trivial_blend_counts() -> Vec<(&'static str, u64)> {
    const NAMES: [&str; 6] = [
        "layer", "screen", "add", "subtract", "multiply", "normal",
    ];
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

/// Drains the allocation tally as a percentage of full-surface targets.
pub fn take_blend_alloc() -> u64 {
    let alloc = BLEND_ALLOC_PX.swap(0, Ordering::Relaxed);
    let full = BLEND_ALLOC_FULL_PX.swap(0, Ordering::Relaxed);
    alloc.saturating_mul(100).checked_div(full).unwrap_or(0)
}

// NOTE: merging complex blend passes was simulated here and REJECTED by the
// measurement (2026-07-22): 20734 passes collapsed into 20350 groups, a 1.02x
// ceiling, because consecutive chunks are pieces of the same avatar and those
// overlap -- and overlapping blends have to stay sequential. The simulation
// counters have been removed along with the idea. Do not rebuild them.

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

/// Drains the coverage tally as `(covered_percent, [hist; 4])`.
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

/// Drains the tally into `mode=count` pairs, busiest first, omitting zeros.
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
    /// Multiply, and only where the destination is known opaque -- see
    /// [`TrivialBlend::blend_state`] for why that condition is the whole
    /// difference between this and a pass of its own.
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
            // Only sound against an opaque destination, which is why multiply
            // lives in `ComplexBlend` as well and why the caller has to prove
            // the condition before choosing this.
            //
            // Setting `dst.a = 1` in `blend/multiply.wgsl` collapses its
            // expression to exactly this state. The shader computes
            //
            //   rgb = src.rgb*(1-dst.a) + dst.rgb*(1-src.a)
            //         + src.a*dst.a * (src.rgb/src.a * dst.rgb/dst.a)
            //   a   = src.a + dst.a*(1-src.a)
            //
            // and at `dst.a = 1` the first term drops out and the third
            // cancels to `src.rgb*dst.rgb`, leaving
            // `src.rgb*dst.rgb + dst.rgb*(1-src.a)` with alpha pinned at 1 --
            // which is `Dst`/`OneMinusSrcAlpha` over `OVER`, term for term.
            // The shader's `src.a == 0` branch discards; here the source is
            // premultiplied, so `src.rgb` is already zero and the same state
            // returns the destination untouched. No approximation anywhere.
            //
            // What is missing over a *transparent* destination is the
            // `src*(1-dst.a)` term, and that is not a rounding difference: it
            // is the whole source, so the art disappears. Hence the condition.
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
