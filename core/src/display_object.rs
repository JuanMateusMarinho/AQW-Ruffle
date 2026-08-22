use crate::avm1::{
    ActivationIdentifier as Avm1ActivationIdentifier, Object as Avm1Object, Value as Avm1Value,
};
use crate::avm2::{
    Activation as Avm2Activation, Avm2, Error as Avm2Error, LoaderInfoObject,
    Multiname as Avm2Multiname, Object as Avm2Object, StageObject as Avm2StageObject, TObject as _,
    Value as Avm2Value,
};
use crate::context::{RenderContext, UpdateContext};
use crate::drawing::Drawing;
use crate::prelude::*;
use crate::string::{AvmString, WString};
use crate::tag_utils::SwfMovie;
use crate::types::{Degrees, Percent};
use crate::vminterface::Instantiator;
use bitflags::bitflags;
use gc_arena::barrier::{Write, unlock};
use gc_arena::lock::Lock;
use gc_arena::{Collect, Gc, Mutation};
use ruffle_macros::{enum_trait_object, istr};
use ruffle_render::perspective_projection::PerspectiveProjection;
use ruffle_render::pixel_bender::PixelBenderShaderHandle;
use ruffle_render::transform::{Transform, TransformStack};
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::fmt::Debug;
use std::hash::Hash;
use std::num::NonZero;
use std::sync::{Arc, OnceLock};
use swf::{ColorTransform, Fixed8};

mod avm1_button;
mod avm2_button;
mod bitmap;
mod container;
mod edit_text;
mod graphic;
mod interactive;
mod loader_display;
mod morph_shape;
mod movie_clip;
mod stage;
mod text;
mod text_line;
mod video;

use crate::avm1::Activation;
use crate::display_object::bitmap::BitmapWeak;
pub use crate::display_object::container::{
    DisplayObjectContainer, TDisplayObjectContainer, dispatch_added_event_only,
    dispatch_added_to_stage_event_only,
};
pub use avm1_button::{Avm1Button, ButtonState, ButtonTracking};
pub use avm2_button::Avm2Button;
pub use bitmap::{Bitmap, BitmapClass};
#[allow(unused)]
pub use edit_text::LayoutDebugBoxesFlag;
pub use edit_text::{AutoSizeMode, EditText, TextSelection};
pub use graphic::Graphic;
pub use interactive::{Avm2MousePick, InteractiveObject, TInteractiveObject};
pub use loader_display::LoaderDisplay;
pub use morph_shape::MorphShape;
pub use movie_clip::{
    MovieClip, MovieClipHandle, MovieClipWeak, Scene, aqw_crt_maybe_toggle, aqw_crt_menu_tick,
    aqw_crt_toggle_external,
};
use ruffle_render::backend::{BitmapCacheEntry, RenderBackend};
use ruffle_render::bitmap::{BitmapHandle, BitmapInfo, PixelSnapping};
use ruffle_render::blend::ExtendedBlendMode;
use ruffle_render::commands::{CommandHandler, CommandList, RenderBlendMode};
use ruffle_render::filters::Filter;
pub use stage::{Stage, StageAlign, StageDisplayState, StageScaleMode, WindowMode};
pub use text::{Text, TextSnapshot};
pub use text_line::TextLine;
pub use video::Video;

use self::loader_display::LoaderDisplayWeak;

/// If a `DisplayObject` is marked `cacheAsBitmap` (via tag or AS),
/// this struct keeps the information required to uphold that cache.
/// A cached Display Object must have its bitmap invalidated when
/// any "visual" change happens, which can include:
/// - Changing the rotation
/// - Changing the scale
/// - Changing the alpha
/// - Changing the color transform
/// - Any "visual" change to children, **including** position changes
///
/// Position changes to the cached Display Object does not regenerate the cache,
/// allowing Display Objects to move freely without being regenerated.
///
/// Flash isn't very good at always recognising when it should be invalidated,
/// and there's cases such as changing the blend mode which don't always trigger it.
///
#[derive(Clone, Debug, Default)]
pub struct BitmapCache {
    /// The `Matrix.a` value that was last used with this cache
    matrix_a: f32,
    /// The `Matrix.b` value that was last used with this cache
    matrix_b: f32,
    /// The `Matrix.c` value that was last used with this cache
    matrix_c: f32,
    /// The `Matrix.d` value that was last used with this cache
    matrix_d: f32,

    /// The width of the original bitmap, pre-filters
    source_width: u32,

    /// The height of the original bitmap, pre-filters
    source_height: u32,

    /// The offset used to draw the final bitmap (i.e. if a filter increases the size)
    draw_offset: Point<i32>,

    /// The current contents of the cache, if any. Values are post-filters.
    bitmap: Option<BitmapInfo>,

    /// Whether we warned that this bitmap was too large to be cached
    warned_for_oversize: bool,

    warned_for_zero: bool,

    stale_anchor: Point<Twips>,

    deferred_frames: u32,

    dirty_streak: u32,

    bake_count: u32,

    static_churn: u32,

    churn_reported: bool,
}

const STATIC_CHURN_REPORT_FRAMES: u32 = 48;

const MAX_CACHE_BITMAP_DIMENSION: u32 = 4096;
const MAX_CACHE_BITMAP_PIXELS: u32 = 2_500_000;

const MAX_AQW_CACHE_BITMAP_SIDE_DEFAULT: u32 = 4096;
const MAX_AQW_CACHE_BITMAP_PIXELS_DEFAULT: u32 = 8_000_000;

fn aqw_max_cache_side() -> u32 {
    static SIDE: OnceLock<u32> = OnceLock::new();
    *SIDE.get_or_init(|| {
        std::env::var("RUFFLE_AQW_CACHE_MAX_SIDE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(MAX_AQW_CACHE_BITMAP_SIDE_DEFAULT)
    })
}

fn aqw_max_cache_pixels() -> u32 {
    static PIXELS: OnceLock<u32> = OnceLock::new();
    *PIXELS.get_or_init(|| {
        std::env::var("RUFFLE_AQW_CACHE_MAX_PIXELS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(MAX_AQW_CACHE_BITMAP_PIXELS_DEFAULT)
    })
}
const CACHE_DEGENERATE_SCALE: f32 = MAX_CACHE_BITMAP_DIMENSION as f32;

pub(crate) fn aqw_env_flag(name: &str, default: bool) -> bool {
    let Some(value) = std::env::var_os(name) else {
        return default;
    };
    let value = value.to_string_lossy();
    let value = value.trim();
    if value.is_empty() {
        return default;
    }
    !(value == "0"
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no"))
}

pub(crate) fn aqw_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_DIAGNOSTICS", false))
}

pub(crate) fn frame_skip_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_FRAME_SKIP", false))
}

pub(crate) fn orphan_pending_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_ORPHAN_PENDING", false))
}

pub(crate) fn script_requeue_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_SCRIPT_REQUEUE", false))
}
/// Deferring a cache redraw composes the previous texture at the previous
/// anchor, so the art on screen stops animating and sits where it used to be.
/// Nothing upstream does this, which is why the artefact is ours alone.
pub(crate) fn redraw_defer_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_REDRAW_DEFER", true))
}

/// When deferral is turned back on, this bounds how old the composed texture
/// may get. The budget escape alone allowed 24 frames - a full second of a
/// frozen, displaced picture.
pub(crate) fn aqw_defer_max_stale_frames() -> u32 {
    static FRAMES: OnceLock<u32> = OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("RUFFLE_AQW_DEFER_MAX_STALE_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(2)
    })
}

pub(crate) fn aqw_defer_eligible_frames() -> u32 {
    static FRAMES: OnceLock<u32> = OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("RUFFLE_AQW_DEFER_ELIGIBLE_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(AQW_DEFER_ELIGIBLE_FRAMES_DEFAULT)
    })
}

const AQW_DEFER_ELIGIBLE_FRAMES_DEFAULT: u32 = 12;

fn aqw_nested_filter_cache_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_NESTED_FILTER_CACHE", false))
}

fn aqw_flicker_probe_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_FLICKER_PROBE", false))
}

fn aqw_flicker_probe_filter() -> Option<&'static str> {
    static FILTER: OnceLock<Option<String>> = OnceLock::new();
    FILTER
        .get_or_init(|| {
            let value = std::env::var("RUFFLE_AQW_FLICKER_PROBE").ok()?;
            (!matches!(value.as_str(), "1" | "true" | "on")).then_some(value)
        })
        .as_deref()
}

fn flicker_probe_covers(url: &str) -> bool {
    match aqw_flicker_probe_filter() {
        Some(filter) => filter
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .any(|part| url.contains(part)),
        None => {
            const NEVER_THE_SUBJECT: [&str; 4] =
                ["charselect", "Game3098", "Loader3", "town-battleon"];
            !NEVER_THE_SUBJECT.iter().any(|part| url.contains(part))
        }
    }
}

fn url_path_has_segment(url: &str, segment: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .split('/')
        .any(|part| part.eq_ignore_ascii_case(segment))
}

fn url_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    Some(authority.split(':').next().unwrap_or(authority))
}

fn is_aqw_movie_url(url: &str) -> bool {
    url_path_has_segment(url, "gamefiles")
}

fn url_basename(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or(url)
}

fn top_counts(items: impl IntoIterator<Item = String>, limit: usize) -> String {
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(limit);
    counts
        .into_iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn aqw_report_orphan_census(context: &UpdateContext<'_>) {
    use crate::avm2::object::TObject;

    let orphans = context.orphan_manager.all_orphans(context.gc());
    let total = orphans.len();
    if total == 0 {
        return;
    }

    let (mut clips, mut constructed, mut frozen, mut quiet, mut playing) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut frozen_inert = 0u64;
    let mut age = [0u64; 4];
    let mut classes: Vec<String> = Vec::with_capacity(total);
    let mut movies: Vec<String> = Vec::with_capacity(total);

    for orphan in &orphans {
        classes.push(
            orphan
                .object2()
                .map(|o| o.instance_class().name().local_name().to_string())
                .unwrap_or_else(|| "<unconstructed>".to_string()),
        );
        movies.push(url_basename(orphan.movie().url()).to_string());
        if orphan.object2().is_some() {
            constructed += 1;
        }
        if let Some(clip) = orphan.as_movie_clip() {
            clips += 1;
            if clip.aqw_orphan_frozen() {
                frozen += 1;
                if clip.aqw_orphan_frozen_inert() {
                    frozen_inert += 1;
                }
            }
            if clip.playing() {
                playing += 1;
            } else if !clip.has_pending_script() {
                quiet += 1;
            }
            let ticks = clip.aqw_orphan_age_ticks();
            let bucket = match ticks {
                0..=23 => 0,
                24..=119 => 1,
                120..=599 => 2,
                _ => 3,
            };
            age[bucket] += 1;
        }
    }

    tracing::info!(
        target: "aqw_diag",
        total,
        clips,
        constructed,
        frozen,
        frozen_inert,
        playing,
        quiet,
        age = format!("<1s:{},<5s:{},<25s:{},older:{}", age[0], age[1], age[2], age[3]).as_str(),
        by_class = top_counts(classes, 8).as_str(),
        by_movie = top_counts(movies, 8).as_str(),
        "AQW orphan census"
    );
}

fn aqw_report_gates(context: &mut UpdateContext<'_>) {
    if !aqw_diagnostics_enabled() {
        return;
    }
    static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let frames = FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if frames < 240 || !(frames - 240).is_multiple_of(1440) {
        return;
    }

    let movie = context.stage.movie();
    let url = movie.url();
    let (mut movies, mut avatar_assets) = (0u32, 0u32);
    for known in context.library.known_movies() {
        movies += 1;
        if movie_clip::is_aqw_avatar_asset_movie_url(known.url()) {
            avatar_assets += 1;
        }
    }
    let line = format!(
        "AQW gates: root_match={} crt_game={} crt_panel={} crt_row={} avatar_assets={avatar_assets}/{movies} root_url={url}",
        is_aqw_movie_url(url),
        movie_clip::aqw_crt_game_name(url),
        movie_clip::aqw_crt_panel_seen(),
        movie_clip::aqw_crt_row_injected(),
    );
    tracing::info!(target: "aqw_diag", "{line}");

    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("aqw-memory.log"))
    {
        let _ = writeln!(file, "{line}");
    }
}

fn aqw_report_stage_children(context: &mut UpdateContext<'_>) {
    let stage = context.stage;

    let describe = |child: DisplayObject<'_>| {
        let kind = match child {
            DisplayObject::LoaderDisplay(_) => "Loader",
            DisplayObject::MovieClip(_) => "MovieClip",
            DisplayObject::Bitmap(_) => "Bitmap",
            DisplayObject::Graphic(_) => "Graphic",
            DisplayObject::EditText(_) => "EditText",
            _ => "other",
        };
        let class = child
            .object2()
            .map(|o| {
                use crate::avm2::object::TObject;
                o.instance_class().name().local_name().to_string()
            })
            .unwrap_or_else(|| "<no avm2 object>".to_string());
        format!("{kind}/{class}")
    };

    static LAST_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let child = stage.child_by_index(0);
    let ptr = child.map(|c| c.as_ptr() as usize).unwrap_or(0);
    if LAST_PTR.swap(ptr, std::sync::atomic::Ordering::Relaxed) == ptr {
        return;
    }

    let current = match child {
        Some(child) => describe(child),
        None => "<empty stage>".to_string(),
    };

    let siblings = stage.num_children();
    let others: Vec<String> = (1..siblings.min(6))
        .filter_map(|index| stage.child_by_index(index).map(describe))
        .collect();
    tracing::warn!(
        "AQW stage probe: getChildAt(0) is now {current} ({siblings} children on stage{})",
        if others.is_empty() {
            String::new()
        } else {
            format!("; others: {}", others.join(", "))
        }
    );
}

impl BitmapCache {
    /// Forcefully make this BitmapCache invalid and require regeneration.
    /// This should be used for changes that aren't automatically detected, such as children.
    pub fn make_dirty(&mut self) {
        // Setting the old transform to something invalid is a cheap way of making it invalid,
        // without reserving an extra field for.
        self.matrix_a = f32::NAN;
    }

    fn note_static_churn(
        &mut self,
        matrix: &Matrix,
        source_width: u32,
        source_height: u32,
        draw_offset: Point<i32>,
    ) -> Option<u32> {
        let matrix_static = self.matrix_a == matrix.a
            && self.matrix_b == matrix.b
            && self.matrix_c == matrix.c
            && self.matrix_d == matrix.d;
        let moved = self.source_width != source_width
            || self.source_height != source_height
            || self.draw_offset != draw_offset;

        if !matrix_static || !moved {
            self.static_churn = 0;
            return None;
        }

        self.static_churn = self.static_churn.saturating_add(1);
        if self.static_churn < STATIC_CHURN_REPORT_FRAMES || self.churn_reported {
            return None;
        }
        self.churn_reported = true;
        Some(self.static_churn)
    }

    fn is_dirty(&self, other: &Matrix, source_width: u32, source_height: u32) -> bool {
        self.matrix_a != other.a
            || self.matrix_b != other.b
            || self.matrix_c != other.c
            || self.matrix_d != other.d
            || self.source_width != source_width
            || self.source_height != source_height
            || self.bitmap.is_none()
    }

    fn dirty_reason(
        &self,
        other: &Matrix,
        source_width: u32,
        source_height: u32,
    ) -> Option<CacheDirtyReason> {
        if self.bitmap.is_none() {
            return Some(CacheDirtyReason::NoTexture);
        }
        if self.source_width != source_width || self.source_height != source_height {
            return Some(CacheDirtyReason::Size);
        }
        if self.matrix_a.is_nan() {
            return Some(CacheDirtyReason::Invalidated);
        }
        if self.matrix_a != other.a
            || self.matrix_b != other.b
            || self.matrix_c != other.c
            || self.matrix_d != other.d
        {
            return Some(CacheDirtyReason::Matrix);
        }
        None
    }

    /// Clears any dirtiness and ensure there's an appropriately sized texture allocated
    #[expect(clippy::too_many_arguments)]
    fn update(
        &mut self,
        renderer: &mut dyn RenderBackend,
        matrix: Matrix,
        source_width: u32,
        source_height: u32,
        actual_width: u32,
        actual_height: u32,
        draw_offset: Point<i32>,
        swf_version: u8,
        allow_aqw_large_cache: bool,
        allow_size_padding: bool,
    ) {
        self.matrix_a = matrix.a;
        self.matrix_b = matrix.b;
        self.matrix_c = matrix.c;
        self.matrix_d = matrix.d;
        self.source_width = source_width;
        self.source_height = source_height;
        self.draw_offset = draw_offset;
        if let Some(current) = &mut self.bitmap {
            if current.width == actual_width && current.height == actual_height {
                return; // No need to resize it
            }
            if allow_size_padding
                && current.width >= actual_width
                && current.height >= actual_height
            {
                let current_pixels = u64::from(current.width) * u64::from(current.height);
                let needed_pixels = u64::from(quantize_cache_dimension(actual_width))
                    * u64::from(quantize_cache_dimension(actual_height));
                if current_pixels <= needed_pixels.saturating_mul(2) {
                    return;
                }
            }
        }
        let total_pixels = actual_width.saturating_mul(actual_height);
        let flash_acceptable_size = if swf_version > 9 {
            actual_width < 8191 && actual_height < 8191 && total_pixels < 16777215
        } else {
            actual_width < 2880 && actual_height < 2880
        };
        let practical_acceptable_size = actual_width <= MAX_CACHE_BITMAP_DIMENSION
            && actual_height <= MAX_CACHE_BITMAP_DIMENSION
            && total_pixels <= MAX_CACHE_BITMAP_PIXELS;
        let aqw_practical_acceptable_size = allow_aqw_large_cache
            && actual_width <= aqw_max_cache_side()
            && actual_height <= aqw_max_cache_side()
            && total_pixels <= aqw_max_cache_pixels();
        let aqw_flash_acceptable_size = allow_aqw_large_cache
            && actual_width < 8191
            && actual_height < 8191
            && total_pixels < 16777215;
        let acceptable_size = (flash_acceptable_size && practical_acceptable_size)
            || (aqw_flash_acceptable_size && aqw_practical_acceptable_size);

        if (aqw_diagnostics_enabled() || aqw_flicker_probe_enabled())
            && !acceptable_size
            && !self.warned_for_oversize
        {
            tracing::warn!(
                target: "aqw_diag",
                source_width,
                source_height,
                actual_width,
                actual_height,
                total_pixels,
                flash_acceptable_size,
                practical_acceptable_size,
                aqw_flash_acceptable_size,
                aqw_practical_acceptable_size,
                "Skipping bitmap cache allocation"
            );
            self.warned_for_oversize = true;
        }

        if aqw_diagnostics_enabled()
            && acceptable_size
            && !(flash_acceptable_size && practical_acceptable_size)
        {
            tracing::info!(
                target: "aqw_diag",
                source_width,
                source_height,
                actual_width,
                actual_height,
                total_pixels,
                "Allowing larger AQW bitmap cache allocation"
            );
        }

        let (alloc_width, alloc_height) = if allow_size_padding {
            (
                quantize_cache_dimension(actual_width),
                quantize_cache_dimension(actual_height),
            )
        } else {
            (actual_width, actual_height)
        };
        if renderer.is_offscreen_supported()
            && let Some(alloc_width) = NonZero::new(alloc_width)
            && let Some(alloc_height) = NonZero::new(alloc_height)
            && acceptable_size
        {
            let handle = renderer.create_empty_texture(alloc_width, alloc_height);
            self.bitmap = handle.ok().map(|handle| BitmapInfo {
                width: alloc_width.get(),
                height: alloc_height.get(),
                handle,
            });
        } else {
            self.bitmap = None;
        }
    }

    /// Explicitly clears the cached value and drops any resources.
    /// This should only be used in situations where you can't render to the cache and it needs to be
    /// temporarily disabled.
    fn clear(&mut self) {
        self.bitmap = None;
        self.dirty_streak = 0;
    }

    fn handle(&self) -> Option<BitmapHandle> {
        self.bitmap.as_ref().map(|b| b.handle.clone())
    }

    fn estimated_bytes(&self) -> u64 {
        self.bitmap
            .as_ref()
            .map_or(0, |b| u64::from(b.width) * u64::from(b.height) * 4)
    }
}

#[derive(Clone, Copy)]
pub struct RenderOptions {
    /// Whether to skip rendering masks.
    ///
    /// Masks are usually skipped when rendering, but when e.g. rendering
    /// the mask itself, it can't be skipped.
    ///
    /// Masks are skipped by default.
    pub skip_masks: bool,

    /// Whether to apply object's base transform.
    ///
    /// For instance, when calling BitmapData.draw, object's transform is not
    /// applied.
    ///
    /// Transform is applied by default.
    pub apply_transform: bool,

    /// Whether to apply base transform's matrix when rendering.
    ///
    /// Sometimes we need to render an object without applying its matrix, but
    /// with applying other parts of its transform (e.g. color transform).
    /// This happens e.g. when rendering alpha masks.
    ///
    /// Matrix is applied by default.
    pub apply_matrix: bool,

    pub apply_scaling_grid: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            apply_transform: true,
            skip_masks: true,
            apply_matrix: true,
            apply_scaling_grid: true,
        }
    }
}

#[derive(Clone, Collect, Debug)]
#[collect(no_drop)]
pub enum RenderMask<'gc> {
    /// There's no mask.
    None,

    /// Stencil masks are the classic, default masks used in Flash Player.
    ///
    /// The masker behaves like a stencil, and masks everything outside its
    /// rendered pixels irrespectively of the pixels themselves.
    /// The maskee acts like being masked with the masker's hit test image.
    Stencil(DisplayObject<'gc>),

    /// Alpha masks are the more advanced (and more intuitive) masks used when
    /// CAB is enabled.
    ///
    /// The maskee is being masked based on the value of the masker's alpha
    /// channel.
    Alpha(DisplayObject<'gc>),
}

#[derive(Clone, Collect)]
#[collect(no_drop)]
// Ensure this always has the same alignment as its subclasses (needed for `Gc` casts).
#[repr(align(8))]
pub struct DisplayObjectBase<'gc> {
    cell: RefCell<DisplayObjectBaseMut>,
    parent: Lock<Option<DisplayObject<'gc>>>,
    place_frame: Cell<u16>,
    depth: Cell<Depth>,
    ratio: Cell<u16>,
    name: Lock<Option<AvmString<'gc>>>,
    clip_depth: Cell<Depth>,

    // The transform of this display object.
    // (Split into several fields for easier access)
    matrix: Cell<Matrix>,
    color_transform: Cell<ColorTransform>,
    perspective_projection: Cell<Option<PerspectiveProjection>>,

    // Cached transform properties `_xscale`, `_yscale`, `_rotation`.
    // These are expensive to calculate, so they will be calculated and cached
    // when AS requests one of these properties.
    rotation: Cell<Degrees>,
    scale_x: Cell<Percent>,
    scale_y: Cell<Percent>,
    skew: Cell<f64>,

    /// The sound transform of sounds playing via this display object.
    sound_transform: Cell<SoundTransform>,

    /// The display object that we are being masked by.
    masker: Lock<Option<DisplayObject<'gc>>>,

    /// The display object we are currently masking.
    maskee: Lock<Option<DisplayObject<'gc>>>,

    meta_data: Lock<Option<Avm2Object<'gc>>>,

    /// The blend mode used when rendering this display object.
    /// Values other than the default `BlendMode::Normal` implicitly cause cache-as-bitmap behavior.
    blend_mode: Cell<ExtendedBlendMode>,

    #[collect(require_static)]

    /// The opaque background color of this display object.
    /// The bounding box of the display object will be filled with the given color. This also
    /// triggers cache-as-bitmap behavior. Only solid backgrounds are supported; the alpha channel
    /// is ignored.
    opaque_background: Cell<Option<Color>>,

    /// Bit flags for various display object properties.
    flags: Cell<DisplayObjectFlags>,

    /// The 'internal' scroll rect used for rendering and methods like 'localToGlobal'.
    /// This is updated from 'pre_render'
    scroll_rect: Cell<Option<Rectangle<Twips>>>,

    /// The 'next' scroll rect, which we will copy to 'scroll_rect' from 'pre_render'.
    /// This is used by the ActionScript 'DisplayObject.scrollRect' getter, which sees
    /// changes immediately (without needing wait for a render)
    next_scroll_rect: Cell<Rectangle<Twips>>,

    /// Rectangle used for 9-slice scaling (`DisplayObject.scale9grid`).
    scaling_grid: Cell<Rectangle<Twips>>,
}

#[derive(Clone)]
struct DisplayObjectBaseMut {
    filters: Box<[Filter]>,

    blend_shader: Option<PixelBenderShaderHandle>,

    /// If this Display Object should cacheAsBitmap - and if so, the cache itself.
    /// None means not cached, Some means cached.
    cache: Option<BitmapCache>,
}

impl Default for DisplayObjectBase<'_> {
    fn default() -> Self {
        Self {
            cell: RefCell::new(DisplayObjectBaseMut {
                filters: Default::default(),
                blend_shader: None,
                cache: None,
            }),
            parent: Default::default(),
            place_frame: Default::default(),
            depth: Default::default(),
            ratio: Default::default(),
            name: Lock::new(None),
            clip_depth: Default::default(),
            matrix: Default::default(),
            color_transform: Default::default(),
            perspective_projection: Default::default(),
            rotation: Cell::new(Degrees::from_radians(0.0)),
            scale_x: Cell::new(Percent::from_unit(1.0)),
            scale_y: Cell::new(Percent::from_unit(1.0)),
            skew: Cell::new(0.0),
            masker: Lock::new(None),
            maskee: Lock::new(None),
            meta_data: Lock::new(None),
            sound_transform: Default::default(),
            blend_mode: Default::default(),
            opaque_background: Default::default(),
            flags: Cell::new(DisplayObjectFlags::VISIBLE | DisplayObjectFlags::SUBTREE_NEEDS_FRAME),
            scroll_rect: Cell::new(None),
            next_scroll_rect: Default::default(),
            scaling_grid: Default::default(),
        }
    }
}

impl<'gc> DisplayObjectBase<'gc> {
    fn contains_flag(&self, flag: DisplayObjectFlags) -> bool {
        self.flags.get().contains(flag)
    }

    fn set_flag(&self, flag: DisplayObjectFlags, value: bool) {
        let mut flags = self.flags.get();
        flags.set(flag, value);
        self.flags.set(flags);
    }

    /// Reset all properties that would be adjusted by a movie load.
    fn reset_for_movie_load(&self) {
        let flags_to_keep = self.flags.get() & DisplayObjectFlags::LOCK_ROOT;
        self.flags.set(
            flags_to_keep | DisplayObjectFlags::VISIBLE | DisplayObjectFlags::SUBTREE_NEEDS_FRAME,
        );
    }

    fn depth(&self) -> Depth {
        self.depth.get()
    }

    fn set_depth(&self, depth: Depth) {
        self.depth.set(depth);
    }

    fn place_frame(&self) -> u16 {
        self.place_frame.get()
    }

    fn set_place_frame(&self, frame: u16) {
        self.place_frame.set(frame);
    }

    fn transform(&self, apply_matrix: bool) -> Transform {
        Transform {
            matrix: if apply_matrix {
                self.matrix.get()
            } else {
                Matrix::IDENTITY
            },
            color_transform: self.color_transform.get(),
            perspective_projection: self.perspective_projection.get(),
        }
    }

    pub fn matrix(&self) -> Matrix {
        self.matrix.get()
    }

    pub fn set_matrix(&self, matrix: Matrix) {
        self.matrix.set(matrix);
        self.set_scale_rotation_cached(false);
    }

    pub fn color_transform(&self) -> ColorTransform {
        self.color_transform.get()
    }

    pub fn set_color_transform(&self, color_transform: ColorTransform) -> bool {
        self.color_transform.replace(color_transform) != color_transform
    }

    pub fn perspective_projection(&self) -> Option<PerspectiveProjection> {
        self.perspective_projection.get()
    }

    pub fn set_perspective_projection(
        &self,
        perspective_projection: Option<PerspectiveProjection>,
    ) -> bool {
        let old = self.perspective_projection.replace(perspective_projection);
        perspective_projection != old
    }

    fn x(&self) -> Twips {
        self.matrix.get().tx
    }

    fn set_x(&self, x: Twips) -> bool {
        let mut matrix = self.matrix.get();
        let changed = matrix.tx != x;
        matrix.tx = x;
        self.matrix.set(matrix);
        self.set_transformed_by_script(true);
        changed
    }

    fn y(&self) -> Twips {
        self.matrix.get().ty
    }

    fn set_y(&self, y: Twips) -> bool {
        let mut matrix = self.matrix.get();
        let changed = matrix.ty != y;
        matrix.ty = y;
        self.matrix.set(matrix);
        self.set_transformed_by_script(true);
        changed
    }

    /// Caches the scale and rotation factors for this display object, if necessary.
    /// Calculating these requires heavy trig ops, so we only do it when `_xscale`, `_yscale` or
    /// `_rotation` is accessed.
    fn cache_scale_rotation(&self) {
        if !self.scale_rotation_cached() {
            let Matrix { a, b, c, d, .. } = self.matrix.get();
            let a = f64::from(a);
            let b = f64::from(b);
            let c = f64::from(c);
            let d = f64::from(d);

            // If this object's transform matrix is:
            // [[a c tx]
            //  [b d ty]]
            // After transformation, the X-axis and Y-axis will turn into the column vectors x' = <a, b> and y' = <c, d>.
            // We derive the scale, rotation, and skew values from these transformed axes.
            // The skew value is not exposed by ActionScript, but is remembered internally.
            // xscale = len(x')
            // yscale = len(y')
            // rotation = atan2(b, a)  (the rotation of x' from the normal x-axis).
            // skew = atan2(-c, d) - atan2(b, a)  (the signed difference between y' and x' rotation)

            // This can produce some surprising results due to the overlap between flipping/rotation/skewing.
            // For example, in Flash, using Modify->Transform->Flip Horizontal and then tracing _xscale, _yscale, and _rotation
            // will output 100, 100, and 180. (a horizontal flip could also be a 180 degree skew followed by 180 degree rotation!)
            let rotation_x = f64::atan2(b, a);
            let rotation_y = f64::atan2(-c, d);
            let scale_x = f64::sqrt(a * a + b * b);
            let scale_y = f64::sqrt(c * c + d * d);
            self.rotation.set(Degrees::from_radians(rotation_x));
            self.scale_x.set(Percent::from_unit(scale_x));
            self.scale_y.set(Percent::from_unit(scale_y));
            self.skew.set(rotation_y - rotation_x);
        }
    }

    fn rotation(&self) -> Degrees {
        self.cache_scale_rotation();
        self.rotation.get()
    }

    fn set_rotation(&self, degrees: Degrees) -> bool {
        self.set_transformed_by_script(true);
        self.cache_scale_rotation();
        let changed = self.rotation.get() != degrees;
        self.rotation.set(degrees);

        // FIXME - this isn't quite correct. In Flash player,
        // trying to set rotation to NaN does nothing if the current
        // matrix 'b' and 'd' terms are both zero. However, if one
        // of those terms is non-zero, then the entire matrix gets
        // modified in a way that depends on its starting values.
        // I haven't been able to figure out how to reproduce those
        // values, so for now, we never modify the matrix if the
        // rotation is NaN. Hopefully, there are no SWFs depending
        // on the weird behavior when b or d is non-zero.
        if degrees.into_radians().is_nan() {
            return changed;
        }

        let skew = self.skew.get();
        let cos_x = f64::cos(degrees.into_radians());
        let sin_x = f64::sin(degrees.into_radians());
        let cos_y = f64::cos(degrees.into_radians() + skew);
        let sin_y = f64::sin(degrees.into_radians() + skew);
        let scale_x = self.scale_x.get().unit();
        let scale_y = self.scale_y.get().unit();
        let mut matrix = self.matrix.get();
        matrix.a = (scale_x * cos_x) as f32;
        matrix.b = (scale_x * sin_x) as f32;
        matrix.c = (scale_y * -sin_y) as f32;
        matrix.d = (scale_y * cos_y) as f32;
        self.matrix.set(matrix);

        changed
    }

    fn scale_x(&self) -> Percent {
        self.cache_scale_rotation();
        self.scale_x.get()
    }

    fn set_scale_x(&self, mut value: Percent) -> bool {
        let changed = self.scale_x.get() != value;
        self.set_transformed_by_script(true);
        self.cache_scale_rotation();
        self.scale_x.set(value);

        // Note - in order to match Flash's behavior, the 'scale_x' field is set to NaN
        // (which gets reported back to ActionScript), but we treat it as 0 for
        // the purposes of updating the matrix
        if value.percent().is_nan() {
            value = 0.0.into();
        }

        // Similarly, a rotation of `NaN` can be reported to ActionScript, but we
        // treat it as 0.0 when calculating the matrix
        let mut rot = self.rotation.get().into_radians();
        if rot.is_nan() {
            rot = 0.0;
        }

        let cos = f64::cos(rot);
        let sin = f64::sin(rot);
        let mut matrix = self.matrix.get();
        matrix.a = (cos * value.unit()) as f32;
        matrix.b = (sin * value.unit()) as f32;
        self.matrix.set(matrix);

        changed
    }

    fn scale_y(&self) -> Percent {
        self.cache_scale_rotation();
        self.scale_y.get()
    }

    fn set_scale_y(&self, mut value: Percent) -> bool {
        let changed = self.scale_y.get() != value;
        self.set_transformed_by_script(true);
        self.cache_scale_rotation();
        self.scale_y.set(value);

        // Note - in order to match Flash's behavior, the 'scale_y' field is set to NaN
        // (which gets reported back to ActionScript), but we treat it as 0 for
        // the purposes of updating the matrix
        if value.percent().is_nan() {
            value = 0.0.into();
        }

        // Similarly, a rotation of `NaN` can be reported to ActionScript, but we
        // treat it as 0.0 when calculating the matrix
        let mut rot = self.rotation.get().into_radians();
        if rot.is_nan() {
            rot = 0.0;
        }

        let skew = self.skew.get();
        let cos = f64::cos(rot + skew);
        let sin = f64::sin(rot + skew);
        let mut matrix = self.matrix.get();
        matrix.c = (-sin * value.unit()) as f32;
        matrix.d = (cos * value.unit()) as f32;
        self.matrix.set(matrix);

        changed
    }

    fn name(&self) -> Option<AvmString<'gc>> {
        self.name.get()
    }

    fn set_name(this: &Write<Self>, name: AvmString<'gc>) {
        unlock!(this, Self, name).set(Some(name));
    }

    fn filters(&self) -> Ref<'_, [Filter]> {
        Ref::map(self.cell.borrow(), |c| &*c.filters)
    }

    fn set_filters(&self, filters: Box<[Filter]>) -> bool {
        let mut write = self.cell.borrow_mut();
        let changed = filters != write.filters;
        write.filters = filters;
        drop(write);
        if changed {
            self.recheck_cache_as_bitmap();
        }
        changed
    }

    fn alpha(&self) -> f64 {
        f64::from(self.color_transform().a_multiply)
    }

    fn set_alpha(&self, value: f64) -> bool {
        self.set_transformed_by_script(true);
        let value = Fixed8::from_f64(value);
        let mut tf = self.color_transform.get();
        let changed = tf.a_multiply != value;
        tf.a_multiply = value;
        self.color_transform.set(tf);
        changed
    }

    fn clip_depth(&self) -> Depth {
        self.clip_depth.get()
    }

    fn set_clip_depth(&self, depth: Depth) {
        self.clip_depth.set(depth);
    }

    fn parent(&self) -> Option<DisplayObject<'gc>> {
        self.parent.get()
    }

    /// You should almost always use `DisplayObject.set_parent` instead, which
    /// properly handles 'orphan' movie clips
    fn set_parent_ignoring_orphan_list(this: &Write<Self>, parent: Option<DisplayObject<'gc>>) {
        unlock!(this, Self, parent).set(parent)
    }

    fn avm1_removed(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::AVM1_REMOVED)
    }

    fn avm1_pending_removal(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::AVM1_PENDING_REMOVAL)
    }

    pub fn should_skip_next_enter_frame(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::SKIP_NEXT_ENTER_FRAME)
    }

    pub fn set_skip_next_enter_frame(&self, skip: bool) {
        self.set_flag(DisplayObjectFlags::SKIP_NEXT_ENTER_FRAME, skip);
    }

    pub fn subtree_needs_frame(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::SUBTREE_NEEDS_FRAME)
    }

    pub fn set_subtree_needs_frame(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::SUBTREE_NEEDS_FRAME, value);
    }

    fn set_avm1_removed(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::AVM1_REMOVED, value);
    }

    fn set_avm1_pending_removal(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::AVM1_PENDING_REMOVAL, value);
    }

    fn scale_rotation_cached(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::SCALE_ROTATION_CACHED)
    }

    fn set_scale_rotation_cached(&self, set_flag: bool) {
        let flags = if set_flag {
            self.flags.get() | DisplayObjectFlags::SCALE_ROTATION_CACHED
        } else {
            self.flags.get() - DisplayObjectFlags::SCALE_ROTATION_CACHED
        };
        self.flags.set(flags);
    }

    pub fn sound_transform(&self) -> SoundTransform {
        self.sound_transform.get()
    }

    pub fn set_sound_transform(&self, sound_transform: SoundTransform) {
        self.sound_transform.set(sound_transform);
    }

    fn visible(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::VISIBLE)
    }

    fn set_visible(&self, value: bool) -> bool {
        let changed = self.visible() != value;
        self.set_flag(DisplayObjectFlags::VISIBLE, value);
        changed
    }

    fn blend_mode(&self) -> ExtendedBlendMode {
        self.blend_mode.get()
    }

    fn set_blend_mode(&self, value: ExtendedBlendMode) -> bool {
        self.blend_mode.replace(value) != value
    }

    fn blend_shader(&self) -> Option<PixelBenderShaderHandle> {
        self.cell.borrow().blend_shader.clone()
    }

    fn set_blend_shader(&self, value: Option<PixelBenderShaderHandle>) {
        self.cell.borrow_mut().blend_shader = value;
    }

    /// The opaque background color of this display object.
    /// The bounding box of the display object will be filled with this color.
    fn opaque_background(&self) -> Option<Color> {
        self.opaque_background.get()
    }

    /// The opaque background color of this display object.
    /// The bounding box of the display object will be filled with the given color. This also
    /// triggers cache-as-bitmap behavior. Only solid backgrounds are supported; the alpha channel
    /// is ignored.
    fn set_opaque_background(&self, value: Option<Color>) -> bool {
        let value = value.map(|mut color| {
            color.a = 255;
            color
        });
        let changed = self.opaque_background.get() != value;
        self.opaque_background.set(value);
        changed
    }

    fn is_root(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::IS_ROOT)
    }

    fn set_is_root(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::IS_ROOT, value);
    }

    fn lock_root(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::LOCK_ROOT)
    }

    fn set_lock_root(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::LOCK_ROOT, value);
    }

    fn transformed_by_script(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::TRANSFORMED_BY_SCRIPT)
    }

    fn set_transformed_by_script(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::TRANSFORMED_BY_SCRIPT, value);
    }

    fn placed_by_avm1_script(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::PLACED_BY_AVM1_SCRIPT)
    }

    fn set_placed_by_avm1_script(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::PLACED_BY_AVM1_SCRIPT, value);
    }

    fn placed_by_avm2_script(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::PLACED_BY_AVM2_SCRIPT)
    }

    fn set_placed_by_avm2_script(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::PLACED_BY_AVM2_SCRIPT, value);
    }

    fn manual_frame_construct(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::MANUAL_FRAME_CONSTRUCT)
    }

    fn set_manual_frame_construct(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::MANUAL_FRAME_CONSTRUCT, value);
    }

    fn is_bitmap_cached_preference(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::CACHE_AS_BITMAP)
    }

    fn set_bitmap_cached_preference(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::CACHE_AS_BITMAP, value);
        self.recheck_cache_as_bitmap();
    }

    fn bitmap_cache_mut(&self) -> RefMut<'_, Option<BitmapCache>> {
        RefMut::map(self.cell.borrow_mut(), |c| &mut c.cache)
    }

    /// Invalidates a cached bitmap, if it exists.
    /// This may only be called once per frame - the first call will return true, regardless of
    /// if there was a cache.
    /// Any subsequent calls will return false, indicating that you do not need to invalidate the ancestors.
    /// This is reset during rendering.
    fn invalidate_cached_bitmap(&self) -> bool {
        if self.contains_flag(DisplayObjectFlags::CACHE_INVALIDATED) {
            return false;
        }
        if let Some(cache) = &mut *self.bitmap_cache_mut() {
            cache.make_dirty();
        }
        self.set_flag(DisplayObjectFlags::CACHE_INVALIDATED, true);
        true
    }

    fn clear_invalidate_flag(&self) {
        self.set_flag(DisplayObjectFlags::CACHE_INVALIDATED, false);
    }

    fn cache_invalidated(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::CACHE_INVALIDATED)
    }

    fn recheck_cache_as_bitmap(&self) {
        let mut write = self.cell.borrow_mut();
        let should_cache = self.is_bitmap_cached_preference() || !write.filters.is_empty();
        if should_cache {
            write.cache.get_or_insert_default();
        } else {
            write.cache = None;
        }
    }

    fn instantiated_by_timeline(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::INSTANTIATED_BY_TIMELINE)
    }

    fn set_instantiated_by_timeline(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::INSTANTIATED_BY_TIMELINE, value);
    }

    fn has_scroll_rect(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::HAS_SCROLL_RECT)
    }

    fn set_has_scroll_rect(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::HAS_SCROLL_RECT, value);
    }

    fn has_explicit_name(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::HAS_EXPLICIT_NAME)
    }

    fn set_has_explicit_name(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::HAS_EXPLICIT_NAME, value);
    }

    fn masker(&self) -> Option<DisplayObject<'gc>> {
        self.masker.get()
    }

    fn set_masker(this: &Write<Self>, node: Option<DisplayObject<'gc>>) {
        unlock!(this, Self, masker).set(node);
    }

    fn maskee(&self) -> Option<DisplayObject<'gc>> {
        self.maskee.get()
    }

    fn set_maskee(this: &Write<Self>, node: Option<DisplayObject<'gc>>) {
        unlock!(this, Self, maskee).set(node);
    }

    fn meta_data(&self) -> Option<Avm2Object<'gc>> {
        self.meta_data.get()
    }

    fn set_meta_data(this: &Write<Self>, value: Avm2Object<'gc>) {
        unlock!(this, Self, meta_data).set(Some(value));
    }

    pub fn has_matrix3d_stub(&self) -> bool {
        self.contains_flag(DisplayObjectFlags::HAS_MATRIX3D_STUB)
    }

    pub fn set_has_matrix3d_stub(&self, value: bool) {
        self.set_flag(DisplayObjectFlags::HAS_MATRIX3D_STUB, value)
    }
}

#[derive(Copy, Clone, Debug)]
pub enum BoundsMode {
    Engine,

    Script,
}

const MAX_DISPLAY_RECURSION_DEPTH: u32 = 512;

thread_local! {
    static RENDER_RECURSION_DEPTH: Cell<u32> = const { Cell::new(0) };
    static BOUNDS_RECURSION_DEPTH: Cell<u32> = const { Cell::new(0) };
    static FRAME_SCRIPT_RECURSION_DEPTH: Cell<u32> = const { Cell::new(0) };
    static LOCAL_FRAME_SCRIPT_RECURSION_DEPTH: Cell<u32> = const { Cell::new(0) };
    static TAB_ORDER_RECURSION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct DisplayObjectRecursionGuard {
    depth: &'static std::thread::LocalKey<Cell<u32>>,
}

impl DisplayObjectRecursionGuard {
    fn enter<'gc>(
        depth: &'static std::thread::LocalKey<Cell<u32>>,
        operation: &'static str,
        this: DisplayObject<'gc>,
    ) -> Option<Self> {
        let current_depth = depth.with(|depth| {
            let current_depth = depth.get().saturating_add(1);
            depth.set(current_depth);
            current_depth
        });

        if current_depth > MAX_DISPLAY_RECURSION_DEPTH {
            depth.with(|depth| depth.set(depth.get().saturating_sub(1)));
            tracing::error!(
                operation = operation,
                depth = current_depth,
                limit = MAX_DISPLAY_RECURSION_DEPTH,
                id = ?this.id(),
                ptr = ?this.as_ptr(),
                name = ?this.name().map(|name| name.to_string()),
                "Display object recursion limit exceeded; skipping recursive operation"
            );
            None
        } else {
            Some(Self { depth })
        }
    }
}

impl Drop for DisplayObjectRecursionGuard {
    fn drop(&mut self) {
        self.depth
            .with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

struct DrawCacheInfo {
    handle: BitmapHandle,
    dirty: bool,
    base_transform: Transform,
    bounds: Rectangle<Twips>,
    draw_offset: Point<i32>,
    filters: Vec<Filter>,
    offset_override: Option<Point<Twips>>,
    aqw_auto_cache: bool,
}

const SCALING_GRID_EPSILON: f32 = 0.0001;
const AQW_OFFSCREEN_CACHE_LIMIT_PIXELS: f64 = 512.0 * 512.0;
const AQW_OFFSCREEN_CACHE_LIMIT_SIDE: f64 = 1024.0;
const AQW_DIRTY_CACHE_REDRAW_DEFER_MIN_PIXELS: u64 = 16_384;
const AQW_DIRTY_CACHE_REDRAW_DEFER_MIN_SIDE: u32 = 128;
const AQW_STALE_CACHE_AGED_FRAMES_DEFAULT: u32 = 24;

fn aqw_stale_cache_aged_frames() -> u32 {
    static FRAMES: OnceLock<u32> = OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("RUFFLE_AQW_STALE_CACHE_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(AQW_STALE_CACHE_AGED_FRAMES_DEFAULT)
    })
}

const AQW_STALE_ANCHOR_MAX_DRIFT_TWIPS: i32 = 16 * 20;

fn aqw_stale_anchor_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_STALE_ANCHOR", false))
}

fn aqw_stale_guard_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_STALE_GUARD", false))
}

fn aqw_drift_norm_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_DRIFT_NORM", false))
}

fn aqw_padded_cache_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_PADDED_CACHE", false))
}

fn quantize_cache_dimension(dim: u32) -> u32 {
    if dim <= 1024 {
        dim.next_multiple_of(32)
    } else {
        dim.next_multiple_of(128)
    }
}

static AQW_VRAM_PRESSURE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn aqw_vram_pressure() -> u8 {
    AQW_VRAM_PRESSURE.load(std::sync::atomic::Ordering::Relaxed)
}

fn aqw_vram_valve_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_VRAM_VALVE", false))
}

use ruffle_render::backend::{
    AQW_POOL_HARD_MB, AQW_POOL_HARD_RELEASE_MB, AQW_POOL_SOFT_MB, AQW_POOL_SOFT_RELEASE_MB,
};

fn update_aqw_vram_pressure(renderer: &mut dyn RenderBackend) -> (u64, u64) {
    use std::sync::atomic::Ordering;

    if aqw_vram_valve_disabled() {
        AQW_VRAM_PRESSURE.store(0, Ordering::Relaxed);
        return (0, 0);
    }
    let (used_mb, budget_mb) = renderer
        .gpu_memory_info()
        .map(|(used, budget)| (used / (1024 * 1024), budget / (1024 * 1024)))
        .unwrap_or((0, 0));

    let Some((_allocs, _frees, retained)) = renderer.offscreen_pool_stats() else {
        AQW_VRAM_PRESSURE.store(0, Ordering::Relaxed);
        return (used_mb, budget_mb);
    };
    let retained_mb = retained / (1024 * 1024);
    let previous = AQW_VRAM_PRESSURE.load(Ordering::Relaxed);
    let level = match previous {
        0 => {
            if retained_mb >= AQW_POOL_HARD_MB {
                2
            } else if retained_mb >= AQW_POOL_SOFT_MB {
                1
            } else {
                0
            }
        }
        1 => {
            if retained_mb >= AQW_POOL_HARD_MB {
                2
            } else if retained_mb < AQW_POOL_SOFT_RELEASE_MB {
                0
            } else {
                1
            }
        }
        _ => {
            if retained_mb < AQW_POOL_SOFT_RELEASE_MB {
                0
            } else if retained_mb < AQW_POOL_HARD_RELEASE_MB {
                1
            } else {
                2
            }
        }
    };
    if level != previous {
        tracing::warn!(
            pool_mb = retained_mb,
            used_mb,
            budget_mb,
            level,
            "GPU memory pressure changed; adjusting cache redraw quotas"
        );
    }
    AQW_VRAM_PRESSURE.store(level, Ordering::Relaxed);
    (used_mb, budget_mb)
}

static AQW_CACHE_REDRAWS_LARGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_REDRAWS_SMALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_REDRAWS_AGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_REDRAWS_DEFERRED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_STALE_FALLBACKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_DEFER_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_STARVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_LAYERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_AVATAR_CACHES_ENABLED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn aqw_avatar_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_AVATAR_CACHE", false))
}

static AQW_BLEND_BY_SWF: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

fn is_complex_blend(mode: ExtendedBlendMode) -> bool {
    matches!(
        mode,
        ExtendedBlendMode::Multiply
            | ExtendedBlendMode::Lighten
            | ExtendedBlendMode::Darken
            | ExtendedBlendMode::Difference
            | ExtendedBlendMode::Invert
            | ExtendedBlendMode::Alpha
            | ExtendedBlendMode::Erase
            | ExtendedBlendMode::Overlay
            | ExtendedBlendMode::HardLight
    )
}

fn note_blend_source(url: &str) {
    let name = url
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url);
    if name.is_empty() {
        return;
    }
    if let Ok(mut guard) = AQW_BLEND_BY_SWF.lock() {
        let counts = guard.get_or_insert_with(std::collections::HashMap::new);
        if counts.len() < 512 || counts.contains_key(name) {
            *counts.entry(name.to_owned()).or_insert(0) += 1;
        }
    }
}

fn take_blend_sources(limit: usize) -> Vec<(String, u64)> {
    let Ok(mut guard) = AQW_BLEND_BY_SWF.lock() else {
        return Vec::new();
    };
    let Some(counts) = guard.take() else {
        return Vec::new();
    };
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts.truncate(limit);
    counts
}

struct BakeTally {
    bakes: u64,
    complex: u64,
    trivial: u64,
}

static AQW_BLEND_BY_BAKE: std::sync::Mutex<Option<std::collections::HashMap<String, BakeTally>>> =
    std::sync::Mutex::new(None);
static AQW_BLEND_BY_LIVE: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);
static AQW_BLEND_BY_LIVE_TRIVIAL: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

static AQW_BLEND_COMPLEX_BAKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_COMPLEX_LIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_TRIVIAL_BAKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_TRIVIAL_LIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BAKE_DEPTH_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static AQW_BAKE_STACK: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn aqw_blend_label<'gc>(this: DisplayObject<'gc>) -> String {
    let class = this
        .object2()
        .map(|o| {
            use crate::avm2::object::TObject;
            o.instance_class().name().local_name().to_string()
        })
        .unwrap_or_else(|| this.name().map(|n| n.to_string()).unwrap_or_default());
    let movie = this.movie();
    let url = movie.url();
    let file = url
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url);
    if class.is_empty() {
        file.to_owned()
    } else {
        format!("{class}@{file}")
    }
}

struct AqwBakeScope(bool);

impl AqwBakeScope {
    fn enter<'gc>(this: DisplayObject<'gc>) -> Self {
        if !aqw_diagnostics_enabled() {
            return Self(false);
        }
        use std::sync::atomic::Ordering::Relaxed;
        AQW_BAKES.fetch_add(1, Relaxed);
        let label = aqw_blend_label(this);
        let depth = AQW_BAKE_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            stack.push(label.clone());
            stack.len() as u64
        });
        AQW_BAKE_DEPTH_MAX.fetch_max(depth, Relaxed);
        if depth == 1
            && let Ok(mut guard) = AQW_BLEND_BY_BAKE.lock()
        {
            let counts = guard.get_or_insert_with(std::collections::HashMap::new);
            if counts.len() < 512 || counts.contains_key(&label) {
                counts
                    .entry(label)
                    .or_insert(BakeTally {
                        bakes: 0,
                        complex: 0,
                        trivial: 0,
                    })
                    .bakes += 1;
            }
        }
        Self(true)
    }
}

impl Drop for AqwBakeScope {
    fn drop(&mut self) {
        if self.0 {
            AQW_BAKE_STACK.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }
}

fn note_blend_emitted<'gc>(this: DisplayObject<'gc>, owner: Option<&str>, complex: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    match (owner.is_some(), complex) {
        (true, true) => AQW_BLEND_COMPLEX_BAKED.fetch_add(1, Relaxed),
        (true, false) => AQW_BLEND_TRIVIAL_BAKED.fetch_add(1, Relaxed),
        (false, true) => AQW_BLEND_COMPLEX_LIVE.fetch_add(1, Relaxed),
        (false, false) => AQW_BLEND_TRIVIAL_LIVE.fetch_add(1, Relaxed),
    };
    match owner {
        Some(owner) => {
            if let Ok(mut guard) = AQW_BLEND_BY_BAKE.lock() {
                let counts = guard.get_or_insert_with(std::collections::HashMap::new);
                if let Some(tally) = counts.get_mut(owner) {
                    if complex {
                        tally.complex += 1;
                    } else {
                        tally.trivial += 1;
                    }
                }
            }
        }
        None => {
            let label = aqw_blend_label(this);
            let tally = if complex {
                note_live_blend_ancestry(this);
                &AQW_BLEND_BY_LIVE
            } else {
                &AQW_BLEND_BY_LIVE_TRIVIAL
            };
            if let Ok(mut guard) = tally.lock() {
                let counts = guard.get_or_insert_with(std::collections::HashMap::new);
                if counts.len() < 512 || counts.contains_key(&label) {
                    *counts.entry(label).or_insert(0) += 1;
                }
            }
        }
    }
}

fn aqw_current_bake() -> Option<String> {
    if !aqw_diagnostics_enabled() {
        return None;
    }
    AQW_BAKE_STACK.with(|stack| stack.borrow().first().cloned())
}

static AQW_CACHE_BYPASS_ALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_BYPASS_ONSCREEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_BYPASS_SAVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BYPASS_BY_OBJECT: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

fn note_cache_bypass<'gc>(
    this: DisplayObject<'gc>,
    context: &RenderContext<'_, 'gc>,
    bounds: &Rectangle<Twips>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    AQW_CACHE_BYPASS_ALL.fetch_add(1, Relaxed);
    let corrected = context.stage.view_matrix() * context.stage.view_bounds();
    let on_screen = bounds.is_valid() && bounds.intersects(&corrected);
    if !on_screen {
        return;
    }
    AQW_CACHE_BYPASS_ONSCREEN.fetch_add(1, Relaxed);
    let label = aqw_blend_label(this);
    if let Ok(mut guard) = AQW_BYPASS_BY_OBJECT.lock() {
        let counts = guard.get_or_insert_with(std::collections::HashMap::new);
        if counts.len() < 512 || counts.contains_key(&label) {
            *counts.entry(label).or_insert(0) += 1;
        }
    }
}

fn take_bypassed_objects(limit: usize) -> Vec<String> {
    let Ok(mut guard) = AQW_BYPASS_BY_OBJECT.lock() else {
        return Vec::new();
    };
    let Some(counts) = guard.take() else {
        return Vec::new();
    };
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts.truncate(limit);
    counts
        .into_iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect()
}

static AQW_BLEND_LIVE_NO_CACHE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_LIVE_HAD_CACHE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

struct LiveAncestorTally {
    blends: u64,
    area_px: u64,
    depth: u32,
}

static AQW_BLEND_LIVE_BY_ANCESTOR: std::sync::Mutex<
    Option<std::collections::HashMap<String, LiveAncestorTally>>,
> = std::sync::Mutex::new(None);

static AQW_CONTENT_DIRTY_FRAMES: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

fn note_content_dirty_frame<'gc>(this: DisplayObject<'gc>) {
    let label = aqw_blend_label(this);
    let Ok(mut guard) = AQW_CONTENT_DIRTY_FRAMES.lock() else {
        return;
    };
    let counts = guard.get_or_insert_with(std::collections::HashMap::new);
    if counts.len() < 512 || counts.contains_key(&label) {
        *counts.entry(label).or_insert(0) += 1;
    }
}

fn take_live_ancestors(limit: usize) -> Vec<String> {
    let dirty = AQW_CONTENT_DIRTY_FRAMES
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .unwrap_or_default();
    let Ok(mut guard) = AQW_BLEND_LIVE_BY_ANCESTOR.lock() else {
        return Vec::new();
    };
    let Some(counts) = guard.take() else {
        return Vec::new();
    };
    let mut counts: Vec<(String, LiveAncestorTally)> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|(_, tally)| std::cmp::Reverse(tally.blends));
    counts.truncate(limit);
    counts
        .into_iter()
        .map(|(name, tally)| {
            format!(
                "{name}:{}/{}k/{}/{}",
                tally.blends,
                tally.area_px / 1000,
                tally.depth,
                dirty.get(&name).copied().unwrap_or(0)
            )
        })
        .collect()
}

#[derive(Copy, Clone)]
pub(crate) enum CacheSuppressReason {
    NotHere,
    Bypassed,
    OversizeU16,
    AllocRefused,
    AllocZero,
    DeferredNoTexture,
    DriftFallback,
    CleanNoTexture,
}

impl CacheSuppressReason {
    fn name(self) -> &'static str {
        match self {
            Self::NotHere => "not_here",
            Self::Bypassed => "bypassed",
            Self::OversizeU16 => "oversize",
            Self::AllocRefused => "alloc_refused",
            Self::AllocZero => "alloc_zero",
            Self::DeferredNoTexture => "defer_notex",
            Self::DriftFallback => "drift",
            Self::CleanNoTexture => "clean_notex",
        }
    }
}

static AQW_CACHE_SUPPRESSED: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

fn note_cache_suppressed<'gc>(
    this: DisplayObject<'gc>,
    reason: CacheSuppressReason,
    detail: Option<&str>,
) {
    let key = match detail {
        Some(detail) => format!("{}|{}[{detail}]", reason.name(), aqw_blend_label(this)),
        None => format!("{}|{}", reason.name(), aqw_blend_label(this)),
    };
    let Ok(mut guard) = AQW_CACHE_SUPPRESSED.lock() else {
        return;
    };
    let counts = guard.get_or_insert_with(std::collections::HashMap::new);
    if counts.len() < 512 || counts.contains_key(&key) {
        *counts.entry(key).or_insert(0) += 1;
    }
}

fn take_cache_suppressed(limit: usize) -> Vec<String> {
    take_live_blend_tally(&AQW_CACHE_SUPPRESSED, limit)
}

fn note_live_blend_ancestry<'gc>(this: DisplayObject<'gc>) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut node = this.parent();
    let mut depth = 0u32;
    let mut had_cache = false;
    while let Some(parent) = node {
        depth += 1;
        note_live_blend_candidate(parent, depth);
        if parent.is_bitmap_cached() {
            AQW_BLEND_LIVE_HAD_CACHE.fetch_add(1, Relaxed);
            had_cache = true;
            break;
        }
        node = parent.parent();
    }
    if !had_cache {
        AQW_BLEND_LIVE_NO_CACHE.fetch_add(1, Relaxed);
    }
}

fn note_live_blend_candidate<'gc>(ancestor: DisplayObject<'gc>, depth: u32) {
    let label = aqw_blend_label(ancestor);
    let Ok(mut guard) = AQW_BLEND_LIVE_BY_ANCESTOR.lock() else {
        return;
    };
    let counts = guard.get_or_insert_with(std::collections::HashMap::new);
    if let Some(tally) = counts.get_mut(&label) {
        tally.blends += 1;
        tally.depth = tally.depth.min(depth);
        return;
    }
    if counts.len() >= 512 {
        return;
    }
    let bounds = ancestor.world_bounds(BoundsMode::Engine);
    let area_px =
        (bounds.width().to_pixels().max(0.0) * bounds.height().to_pixels().max(0.0)) as u64;
    counts.insert(
        label,
        LiveAncestorTally {
            blends: 1,
            area_px,
            depth,
        },
    );
}

fn take_bake_blends(limit: usize) -> Vec<String> {
    let Ok(mut guard) = AQW_BLEND_BY_BAKE.lock() else {
        return Vec::new();
    };
    let Some(counts) = guard.take() else {
        return Vec::new();
    };
    let mut counts: Vec<(String, BakeTally)> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|(_, tally)| std::cmp::Reverse(tally.complex));
    counts.truncate(limit);
    counts
        .into_iter()
        .map(|(name, tally)| format!("{name}:{}/{}", tally.bakes, tally.complex))
        .collect()
}

fn take_live_blends(limit: usize) -> Vec<String> {
    take_live_blend_tally(&AQW_BLEND_BY_LIVE, limit)
}

fn take_live_trivial_blends(limit: usize) -> Vec<String> {
    take_live_blend_tally(&AQW_BLEND_BY_LIVE_TRIVIAL, limit)
}

fn take_live_blend_tally(
    tally: &std::sync::Mutex<Option<std::collections::HashMap<String, u64>>>,
    limit: usize,
) -> Vec<String> {
    let Ok(mut guard) = tally.lock() else {
        return Vec::new();
    };
    let Some(counts) = guard.take() else {
        return Vec::new();
    };
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts.truncate(limit);
    counts
        .into_iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect()
}
pub(crate) static AQW_TICK_ORPHAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_TICK_STAGE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_TICK_BCAST_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHANS_FROZEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Copy, Clone)]
pub(crate) enum CacheDirtyReason {
    NoTexture,
    Size,
    Invalidated,
    Matrix,
}

static AQW_BAKE_REPEAT_HIST: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static AQW_BAKE_REPEAT_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn aqw_note_bake_repeat(bake_count: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    let bucket = match bake_count {
        0..=1 => 0,
        2..=4 => 1,
        5..=24 => 2,
        25..=120 => 3,
        _ => 4,
    };
    AQW_BAKE_REPEAT_HIST[bucket].fetch_add(1, Relaxed);
    AQW_BAKE_REPEAT_MAX.fetch_max(u64::from(bake_count), Relaxed);
}

fn take_bake_repeats() -> ([u64; 5], u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let hist = std::array::from_fn(|i| AQW_BAKE_REPEAT_HIST[i].swap(0, Relaxed));
    (hist, AQW_BAKE_REPEAT_MAX.load(Relaxed))
}

pub(crate) static AQW_CACHE_DIRTY_NOTEX: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_CACHE_DIRTY_SIZE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_CACHE_DIRTY_MATRIX: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_CACHE_DIRTY_INVALID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_CACHE_DEFER_GRACE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_CACHE_BYPASS_CLEARED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

static AQW_INVALIDATE_DEPTH: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

static AQW_INVALIDATE_ORIGIN: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

thread_local! {
    static AQW_INVALIDATE_ORIGIN_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn aqw_note_invalidation(origin: DisplayObject<'_>) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut node = Some(origin);
    let mut depth = 0u32;
    let mut reached_ancestor = false;
    while let Some(current) = node {
        if current.is_bitmap_cached() {
            let bucket = match depth {
                0 => 0,
                1 => 1,
                2..=3 => 2,
                _ => 3,
            };
            AQW_INVALIDATE_DEPTH[bucket].fetch_add(1, Relaxed);
            if depth > 0 {
                reached_ancestor = true;
            }
        }
        depth += 1;
        node = current.parent();
    }
    if !reached_ancestor {
        return;
    }
    let label = aqw_blend_label(origin);
    if let Ok(mut guard) = AQW_INVALIDATE_ORIGIN.lock() {
        let counts = guard.get_or_insert_with(std::collections::HashMap::new);
        if counts.len() < 512 || counts.contains_key(&label) {
            *counts.entry(label).or_insert(0) += 1;
        }
    }
}

fn take_invalidate_depth() -> [u64; 4] {
    use std::sync::atomic::Ordering::Relaxed;
    std::array::from_fn(|i| AQW_INVALIDATE_DEPTH[i].swap(0, Relaxed))
}

fn take_invalidate_origins(limit: usize) -> Vec<String> {
    take_live_blend_tally(&AQW_INVALIDATE_ORIGIN, limit)
}

pub(crate) static AQW_ORPHAN_PROBE_EXHAUSTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_ORPHAN_POP: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_POP_PLAYING: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_POP_SCRIPT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_POP_AVATAR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_POP_QUIET: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_CLEAN_VISITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_DIRTY_VISITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_SCRIPT_REARM_SKIPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_CLEAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_DIRTY_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn aqw_note_orphan_visit(clean: bool, elapsed_ns: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if clean {
        AQW_ORPHAN_CLEAN_VISITS.fetch_add(1, Relaxed);
        AQW_ORPHAN_CLEAN_NS.fetch_add(elapsed_ns, Relaxed);
    } else {
        AQW_ORPHAN_DIRTY_VISITS.fetch_add(1, Relaxed);
        AQW_ORPHAN_DIRTY_NS.fetch_add(elapsed_ns, Relaxed);
    }
}

pub(crate) static AQW_CLIPS_ADVANCED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_RUNFRAME_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_BCAST_ENTER_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_GOTO_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_GOTO_SCAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_REMOVE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_APPLY_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_TAGS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_GOTO_FAST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn aqw_goto_fastpath_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_GOTO_FASTPATH", false))
}

pub(crate) static AQW_GOTO_PLACE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_INNER_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_REWINDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_FRAMES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

static AQW_LISTENER_COST: std::sync::Mutex<Option<std::collections::HashMap<String, (u64, u64)>>> =
    std::sync::Mutex::new(None);

pub(crate) fn aqw_note_listener_cost(class: String, ns: u64) {
    if let Ok(mut guard) = AQW_LISTENER_COST.lock() {
        let entry = guard
            .get_or_insert_with(Default::default)
            .entry(class)
            .or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ns;
    }
}

fn aqw_listener_cost_top(n: usize) -> String {
    let Ok(mut guard) = AQW_LISTENER_COST.lock() else {
        return String::new();
    };
    let Some(map) = guard.as_mut() else {
        return String::new();
    };
    let mut rows: Vec<_> = map
        .iter()
        .map(|(class, (calls, ns))| (ns / 1_000, *calls, class.clone()))
        .collect();
    map.clear();
    rows.sort_unstable_by_key(|&(us, ..)| std::cmp::Reverse(us));
    rows.truncate(n);
    rows.iter()
        .map(|(us, calls, class)| format!("{class}={us}us/{calls}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) static AQW_INNER_ORPHAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_INNER_STAGE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_INNER_BCAST_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_INNER_CLEANUP_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_INNER_FRAMES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_GATE_OPEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_ADDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_MARK_BUMPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_INNER_ORPHAN_VISITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_INNER_ORPHAN_WORK: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_UNQUEUE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_PLACE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_PLACE_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_RENDERLIST_COPIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_RENDERLIST_COPIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_STAGE_ENTER_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_STAGE_CTOR_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_STAGE_SCRIPT_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static AQW_HOOK_CLIPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_HOOK_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn aqw_throttle_hook_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_THROTTLE_HOOK", false))
}

fn scale_twips(value: Twips, scale: f32) -> Twips {
    Twips::new((value.get() as f32 * scale).round_ties_even() as i32)
}

fn slice_matrix(
    src_x0: Twips,
    src_x1: Twips,
    dst_x0: Twips,
    dst_x1: Twips,
    src_y0: Twips,
    src_y1: Twips,
    dst_y0: Twips,
    dst_y1: Twips,
) -> Option<Matrix> {
    let src_w = src_x1 - src_x0;
    let src_h = src_y1 - src_y0;
    let dst_w = dst_x1 - dst_x0;
    let dst_h = dst_y1 - dst_y0;

    if src_w <= Twips::ZERO || src_h <= Twips::ZERO || dst_w <= Twips::ZERO || dst_h <= Twips::ZERO
    {
        return None;
    }

    let scale_x = dst_w.get() as f32 / src_w.get() as f32;
    let scale_y = dst_h.get() as f32 / src_h.get() as f32;

    Some(Matrix {
        a: scale_x,
        b: 0.0,
        c: 0.0,
        d: scale_y,
        tx: dst_x0 - scale_twips(src_x0, scale_x),
        ty: dst_y0 - scale_twips(src_y0, scale_y),
    })
}

fn aqw_scale9_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_SCALE9", false))
}

thread_local! {
    static LAST_AQW_RESIZE: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
}

const AQW_RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);

pub fn note_aqw_stage_resize() {
    LAST_AQW_RESIZE.with(|cell| cell.set(Some(std::time::Instant::now())));
}

fn aqw_recently_resized() -> bool {
    LAST_AQW_RESIZE.with(|cell| match cell.get() {
        None => false,
        Some(at) if at.elapsed() < AQW_RESIZE_SETTLE => true,
        Some(_) => {
            cell.set(None);
            false
        }
    })
}

fn render_aqw_scaling_grid<'gc>(
    this: DisplayObject<'gc>,
    context: &mut RenderContext<'_, 'gc>,
    options: RenderOptions,
) -> bool {
    if aqw_scale9_disabled() || aqw_recently_resized() {
        return false;
    }
    if !options.apply_transform || !options.apply_matrix || !options.apply_scaling_grid {
        return false;
    }

    let grid = this.scaling_grid();
    if !grid.is_valid() {
        return false;
    }

    if !is_aqw_movie_url(this.movie().url()) {
        return false;
    }

    let base_transform = this.base().transform(true);
    let parent_transform = context.transform_stack.transform();
    let parent_matrix = parent_transform.matrix;
    let object_matrix = base_transform.matrix;

    if object_matrix.b.abs() > SCALING_GRID_EPSILON
        || object_matrix.c.abs() > SCALING_GRID_EPSILON
        || object_matrix.a <= SCALING_GRID_EPSILON
        || object_matrix.d <= SCALING_GRID_EPSILON
    {
        return false;
    }

    if (object_matrix.a - 1.0).abs() <= SCALING_GRID_EPSILON
        && (object_matrix.d - 1.0).abs() <= SCALING_GRID_EPSILON
    {
        return false;
    }

    let bounds = this.bounds(BoundsMode::Engine).union(&grid);
    if !bounds.is_valid()
        || bounds.width() <= Twips::ZERO
        || bounds.height() <= Twips::ZERO
        || grid.x_min >= grid.x_max
        || grid.y_min >= grid.y_max
    {
        return false;
    }

    let dst_bounds = object_matrix * bounds;
    if !dst_bounds.is_valid()
        || dst_bounds.width() <= Twips::ZERO
        || dst_bounds.height() <= Twips::ZERO
    {
        return false;
    }

    let left_width = grid.x_min - bounds.x_min;
    let right_width = bounds.x_max - grid.x_max;
    let top_height = grid.y_min - bounds.y_min;
    let bottom_height = bounds.y_max - grid.y_max;

    let dst_x = [
        dst_bounds.x_min,
        dst_bounds.x_min + left_width,
        dst_bounds.x_max - right_width,
        dst_bounds.x_max,
    ];
    let dst_y = [
        dst_bounds.y_min,
        dst_bounds.y_min + top_height,
        dst_bounds.y_max - bottom_height,
        dst_bounds.y_max,
    ];

    if dst_x[1] >= dst_x[2] || dst_y[1] >= dst_y[2] {
        return false;
    }

    let src_x = [bounds.x_min, grid.x_min, grid.x_max, bounds.x_max];
    let src_y = [bounds.y_min, grid.y_min, grid.y_max, bounds.y_max];
    let old_use_bitmap_cache = context.use_bitmap_cache;
    context.use_bitmap_cache = false;
    let old_cache_filtered_children = context.cache_filtered_children;
    context.cache_filtered_children = false;

    let mut slice_options = options;
    slice_options.apply_transform = false;
    slice_options.apply_scaling_grid = false;

    for y in 0..3 {
        for x in 0..3 {
            let Some(matrix) = slice_matrix(
                src_x[x],
                src_x[x + 1],
                dst_x[x],
                dst_x[x + 1],
                src_y[y],
                src_y[y + 1],
                dst_y[y],
                dst_y[y + 1],
            ) else {
                continue;
            };

            let mask_rect = Rectangle {
                x_min: dst_x[x],
                x_max: dst_x[x + 1],
                y_min: dst_y[y],
                y_max: dst_y[y + 1],
            };
            let mask_matrix = parent_matrix * Matrix::create_box_from_rectangle(&mask_rect);

            context.commands.push_mask();
            context.commands.draw_rect(Color::WHITE, mask_matrix);
            context.commands.activate_mask();

            context.transform_stack.push(&Transform {
                matrix,
                color_transform: base_transform.color_transform,
                perspective_projection: base_transform.perspective_projection,
            });
            this.render_with_options(context, slice_options);
            context.transform_stack.pop();

            context.commands.deactivate_mask();
            context.commands.draw_rect(Color::WHITE, mask_matrix);
            context.commands.pop_mask();
        }
    }

    context.use_bitmap_cache = old_use_bitmap_cache;
    context.cache_filtered_children = old_cache_filtered_children;
    true
}

fn should_bypass_offscreen_bitmap_cache<'gc>(
    this: DisplayObject<'gc>,
    context: &RenderContext<'_, 'gc>,
    options: RenderOptions,
    bounds: &Rectangle<Twips>,
    filters: &[Filter],
) -> bool {
    if context.is_offscreen
        || context.commands.drawing_mask()
        || !options.skip_masks
        || this.is_root()
        || this.clip_depth() > 0
        || this.maskee().is_some()
        || this.masker().is_some()
        || this.scroll_rect().is_some()
        || this.opaque_background().is_some()
        || this.blend_mode() != ExtendedBlendMode::Normal
        || !filters.is_empty()
        || !bounds.is_valid()
    {
        return false;
    }

    let width = bounds.width().to_pixels().ceil().max(0.0);
    let height = bounds.height().to_pixels().ceil().max(0.0);

    if width * height < AQW_OFFSCREEN_CACHE_LIMIT_PIXELS
        && width < AQW_OFFSCREEN_CACHE_LIMIT_SIDE
        && height < AQW_OFFSCREEN_CACHE_LIMIT_SIDE
    {
        return false;
    }

    let stage_view_bounds = context.stage.view_bounds();
    let on_screen = if aqw_bypass_view_fix_disabled() {
        bounds.intersects(&stage_view_bounds)
    } else {
        bounds.intersects(&(context.stage.view_matrix() * stage_view_bounds))
    };

    if aqw_diagnostics_enabled() && on_screen && !bounds.intersects(&stage_view_bounds) {
        AQW_CACHE_BYPASS_SAVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    !on_screen
}

fn aqw_bypass_view_fix_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_BYPASS_VIEW_FIX", false))
}

fn aqw_cache_budget_mb() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("RUFFLE_AQW_CACHE_BUDGET_MB")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0)
    })
}

pub(crate) static AQW_TIMELINE_STALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn aqw_stall_probe_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_STALL_PROBE", false))
}

thread_local! {
    /// ptr -> (last frame seen, ticks stalled, ticks since we last reported it)
    static AQW_STALL_STATE: RefCell<std::collections::HashMap<usize, (u16, u16, u16)>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Looks for on-stage avatar art whose timeline is standing still, and names
/// whatever in this fork could be holding it: the throttle, either freeze, the
/// frame-skip mark, or the orphan list.
fn aqw_report_timeline_stalls(context: &mut UpdateContext<'_>) {
    fn visit<'gc>(node: DisplayObject<'gc>, context: &UpdateContext<'gc>, budget: &mut u32) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;

        if let Some(clip) = node.as_movie_clip()
            && clip.playing()
            && clip.header_frames() > 1
            && is_aqw_movie_url(clip.movie().url())
        {
            let ptr = node.as_ptr() as usize;
            let frame = clip.current_frame();
            let report = AQW_STALL_STATE.with(|state| {
                let mut state = state.borrow_mut();
                let entry = state.entry(ptr).or_insert((frame, 0, 0));
                if entry.0 == frame {
                    entry.1 = entry.1.saturating_add(1);
                } else {
                    entry.0 = frame;
                    entry.1 = 0;
                }
                entry.2 = entry.2.saturating_add(1);
                if entry.1 >= 8 && entry.2 >= 48 {
                    entry.2 = 0;
                    Some(entry.1)
                } else {
                    None
                }
            });
            if let Some(stalled) = report {
                AQW_TIMELINE_STALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let class = node
                    .object2()
                    .map(|o| {
                        use crate::avm2::object::TObject;
                        o.instance_class().name().local_name().to_string()
                    })
                    .unwrap_or_else(|| "<unconstructed>".to_string());
                tracing::warn!(
                    target: "aqw_diag",
                    class = class.as_str(),
                    stalled_ticks = stalled,
                    frame,
                    total_frames = clip.header_frames(),
                    on_stage = clip.is_on_stage(context),
                    frozen = clip.aqw_orphan_frozen(),
                    frozen_inert = clip.aqw_orphan_frozen_inert(),
                    subtree_needs_frame = !node.aqw_subtree_clean(),
                    parent = node.parent().is_some(),
                    visible = node.visible(),
                    movie = url_basename(node.movie().url()),
                    "AQW timeline stalled"
                );
            }
        }

        if let Some(container) = node.as_container() {
            for child in container.iter_render_list() {
                visit(child, context, budget);
            }
        }
    }

    let mut budget = 20000u32;
    let stage: DisplayObject<'_> = context.stage.into();
    visit(stage, context, &mut budget);
}

pub fn aqw_cache_sweep(context: &mut UpdateContext<'_>) {
    if aqw_stall_probe_enabled() {
        aqw_report_timeline_stalls(context);
    }
    let diagnostics = aqw_diagnostics_enabled();
    let budget_mb = aqw_cache_budget_mb();
    aqw_report_gates(context);
    aqw_report_stage_children(context);
    if !is_aqw_movie_url(context.stage.movie().url()) {
        return;
    }

    static SWEEP_FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if !SWEEP_FRAME
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .is_multiple_of(48)
    {
        return;
    }

    let (vram_mb, vram_budget_mb) = update_aqw_vram_pressure(context.renderer);

    if !diagnostics && budget_mb == 0 {
        return;
    }

    static LAST_CACHE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let budget_bytes = budget_mb.saturating_mul(1024 * 1024);
    let over_budget = budget_bytes > 0
        && LAST_CACHE_BYTES.load(std::sync::atomic::Ordering::Relaxed) > budget_bytes;

    let view_bounds = context.stage.view_bounds();
    let root: DisplayObject<'_> = context.stage.into();

    let mut cached_objects: u64 = 0;
    let mut cache_bytes: u64 = 0;
    let mut evicted_bytes: u64 = 0;
    aqw_sweep_node(
        root,
        &view_bounds,
        over_budget,
        &mut cached_objects,
        &mut cache_bytes,
        &mut evicted_bytes,
    );
    LAST_CACHE_BYTES.store(cache_bytes, std::sync::atomic::Ordering::Relaxed);

    if diagnostics {
        aqw_report_orphan_census(context);
        let orphans = context.orphan_manager.len();
        let loaders = context.load_manager.len();
        let cache_mb = cache_bytes / (1024 * 1024);
        let evicted_mb = evicted_bytes / (1024 * 1024);
        let movie_libs_total = context.library.known_movies().count();
        let movie_libs_aqw = context
            .library
            .known_movies()
            .filter(|m| is_aqw_movie_url(m.url()))
            .count();
        let avatar_roots = *context.aqw_avatar_asset_roots_previous;
        use std::sync::atomic::Ordering;
        let redraw_large = AQW_CACHE_REDRAWS_LARGE.swap(0, Ordering::Relaxed);
        let redraw_small = AQW_CACHE_REDRAWS_SMALL.swap(0, Ordering::Relaxed);
        let redraw_aged = AQW_CACHE_REDRAWS_AGED.swap(0, Ordering::Relaxed);
        let redraw_deferred = AQW_CACHE_REDRAWS_DEFERRED.swap(0, Ordering::Relaxed);
        let stale_fallback = AQW_CACHE_STALE_FALLBACKS.swap(0, Ordering::Relaxed);
        let defer_max = AQW_CACHE_DEFER_MAX.swap(0, Ordering::Relaxed);
        let starved = AQW_CACHE_STARVED.swap(0, Ordering::Relaxed);
        let blend_layers = AQW_BLEND_LAYERS.swap(0, Ordering::Relaxed);
        let tick_orphan_ms = AQW_TICK_ORPHAN_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let tick_stage_ms = AQW_TICK_STAGE_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let tick_bcast_ms = AQW_TICK_BCAST_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let orphans_frozen = AQW_ORPHANS_FROZEN.swap(0, Ordering::Relaxed);
        let dirty_notex = AQW_CACHE_DIRTY_NOTEX.swap(0, Ordering::Relaxed);
        let dirty_size = AQW_CACHE_DIRTY_SIZE.swap(0, Ordering::Relaxed);
        let dirty_matrix = AQW_CACHE_DIRTY_MATRIX.swap(0, Ordering::Relaxed);
        let dirty_invalid = AQW_CACHE_DIRTY_INVALID.swap(0, Ordering::Relaxed);
        let defer_grace = AQW_CACHE_DEFER_GRACE.swap(0, Ordering::Relaxed);
        let bypass_cleared = AQW_CACHE_BYPASS_CLEARED.swap(0, Ordering::Relaxed);
        let orph_pop = AQW_ORPHAN_POP.swap(0, Ordering::Relaxed);
        let orph_playing = AQW_ORPHAN_POP_PLAYING.swap(0, Ordering::Relaxed);
        let orph_script = AQW_ORPHAN_POP_SCRIPT.swap(0, Ordering::Relaxed);
        let orph_avatar = AQW_ORPHAN_POP_AVATAR.swap(0, Ordering::Relaxed);
        let orph_quiet = AQW_ORPHAN_POP_QUIET.swap(0, Ordering::Relaxed);
        let invalidate_depth = take_invalidate_depth()
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("/");
        let dirty_origin = take_invalidate_origins(8).join(",");
        let orph_probe_out = AQW_ORPHAN_PROBE_EXHAUSTED.swap(0, Ordering::Relaxed);
        let orph_clean_visits = AQW_ORPHAN_CLEAN_VISITS.swap(0, Ordering::Relaxed);
        let orph_dirty_visits = AQW_ORPHAN_DIRTY_VISITS.swap(0, Ordering::Relaxed);
        let orph_clean_ms = AQW_ORPHAN_CLEAN_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let orph_dirty_ms = AQW_ORPHAN_DIRTY_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let script_rearm_skip = AQW_SCRIPT_REARM_SKIPPED.swap(0, Ordering::Relaxed);
        let timeline_stalls = AQW_TIMELINE_STALLS.swap(0, Ordering::Relaxed);
        let aqw_hook_ms = AQW_HOOK_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let aqw_clips = AQW_HOOK_CLIPS.swap(0, Ordering::Relaxed);
        let aqw_playing = AQW_CLIPS_ADVANCED.swap(0, Ordering::Relaxed);
        let runframe_ms = AQW_RUNFRAME_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let bcast_enter_ms = AQW_BCAST_ENTER_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let (listeners, listeners_susp, bcast_max, bcast_max_event) =
            context.avm2.broadcast_stats();
        let goto_ms = AQW_GOTO_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let goto_calls = AQW_GOTO_CALLS.swap(0, Ordering::Relaxed);
        let goto_rewinds = AQW_GOTO_REWINDS.swap(0, Ordering::Relaxed);
        let goto_frames = AQW_GOTO_FRAMES.swap(0, Ordering::Relaxed);
        let goto_scan_ms = AQW_GOTO_SCAN_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let goto_remove_ms = AQW_GOTO_REMOVE_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let goto_apply_ms = AQW_GOTO_APPLY_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let goto_tags = AQW_GOTO_TAGS.swap(0, Ordering::Relaxed);
        let goto_fast = AQW_GOTO_FAST.swap(0, Ordering::Relaxed);
        let goto_place_ms = AQW_GOTO_PLACE_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let goto_inner_ms = AQW_GOTO_INNER_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let bcast_top = aqw_listener_cost_top(6);
        let inner_orphan_ms = AQW_INNER_ORPHAN_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let inner_stage_ms = AQW_INNER_STAGE_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let inner_bcast_ms = AQW_INNER_BCAST_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let inner_cleanup_ms = AQW_INNER_CLEANUP_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let inner_frames = AQW_INNER_FRAMES.swap(0, Ordering::Relaxed);
        let inner_orphan_visits = AQW_INNER_ORPHAN_VISITS.swap(0, Ordering::Relaxed);
        let inner_orphan_work = AQW_INNER_ORPHAN_WORK.swap(0, Ordering::Relaxed);
        let gate_open = AQW_GATE_OPEN.swap(0, Ordering::Relaxed);
        let orphan_adds = AQW_ORPHAN_ADDS.swap(0, Ordering::Relaxed);
        let mark_bumps = AQW_MARK_BUMPS.swap(0, Ordering::Relaxed);
        let unqueue_ms = AQW_UNQUEUE_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let place_ms = AQW_PLACE_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let places = AQW_PLACE_COUNT.swap(0, Ordering::Relaxed);
        let list_copies = AQW_RENDERLIST_COPIES.swap(0, Ordering::Relaxed);
        let list_copied = AQW_RENDERLIST_COPIED.swap(0, Ordering::Relaxed);
        let stage_enter_ms = AQW_STAGE_ENTER_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let stage_ctor_ms = AQW_STAGE_CTOR_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let stage_script_ms = AQW_STAGE_SCRIPT_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let vram_pressure = aqw_vram_pressure();
        static LAST_POOL_ALLOCS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static LAST_POOL_FREES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (pool_allocs, pool_free, pool_mb) =
            if let Some((allocs, frees, retained)) = context.renderer.offscreen_pool_stats() {
                (
                    allocs.saturating_sub(LAST_POOL_ALLOCS.swap(allocs, Ordering::Relaxed)),
                    frees.saturating_sub(LAST_POOL_FREES.swap(frees, Ordering::Relaxed)),
                    retained / (1024 * 1024),
                )
            } else {
                (0, 0, 0)
            };
        static LAST_TEX_ALLOCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static LAST_TEX_FREES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (tex_allocs, tex_free, tex_mb) =
            if let Some((allocs, frees, retained)) = context.renderer.surface_pool_stats() {
                (
                    allocs.saturating_sub(LAST_TEX_ALLOCS.swap(allocs, Ordering::Relaxed)),
                    frees.saturating_sub(LAST_TEX_FREES.swap(frees, Ordering::Relaxed)),
                    retained / (1024 * 1024),
                )
            } else {
                (0, 0, 0)
            };
        let tex_top = context
            .renderer
            .surface_pool_largest(4)
            .iter()
            .map(|(w, h, count, bytes)| format!("{w}x{h}x{count}={}MB", bytes / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(",");

        let blend_modes = context
            .renderer
            .take_complex_blend_counts()
            .iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(",");

        let blend_alloc = context.renderer.take_blend_alloc();
        let (blend_mul_opaque, blend_mul_total, blend_mul_opaque_mpx) =
            context.renderer.take_blend_dest_opacity();
        let blend_mul_opaque =
            format!("{blend_mul_opaque}/{blend_mul_total} ({blend_mul_opaque_mpx}Mpx)");
        let (render_encode_ms, render_submit_ms, render_frames, commit_mb) =
            context.renderer.take_render_timings();
        let avatar_caches = AQW_AVATAR_CACHES_ENABLED.load(Ordering::Relaxed);
        let blend_swfs = take_blend_sources(6)
            .iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(",");
        let blend_bake = AQW_BLEND_COMPLEX_BAKED.swap(0, Ordering::Relaxed);
        let blend_live = AQW_BLEND_COMPLEX_LIVE.swap(0, Ordering::Relaxed);
        let blend_bake_triv = AQW_BLEND_TRIVIAL_BAKED.swap(0, Ordering::Relaxed);
        let blend_live_triv = AQW_BLEND_TRIVIAL_LIVE.swap(0, Ordering::Relaxed);
        let bakes = AQW_BAKES.swap(0, Ordering::Relaxed);
        let (bake_reps, bake_reps_max) = take_bake_repeats();
        let bake_reps = bake_reps
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("/");
        let bake_depth = AQW_BAKE_DEPTH_MAX.swap(0, Ordering::Relaxed);
        let bake_top = take_bake_blends(6).join(",");
        let blend_live_top = take_live_blends(6).join(",");
        let bypass_all = AQW_CACHE_BYPASS_ALL.swap(0, Ordering::Relaxed);
        let bypass_onscreen = AQW_CACHE_BYPASS_ONSCREEN.swap(0, Ordering::Relaxed);
        let bypass_saved = AQW_CACHE_BYPASS_SAVED.swap(0, Ordering::Relaxed);
        let bypass_top = take_bypassed_objects(6).join(",");
        let live_nocache = AQW_BLEND_LIVE_NO_CACHE.swap(0, Ordering::Relaxed);
        let live_hadcache = AQW_BLEND_LIVE_HAD_CACHE.swap(0, Ordering::Relaxed);
        let live_cand = take_live_ancestors(8).join(",");
        let cache_suppress = take_cache_suppressed(8).join(",");
        let (dicts, dict_okeys) = crate::avm2::object::dictionary_stats();
        let (bmp_tex, bmp_mb) = context
            .renderer
            .bitmap_texture_stats()
            .map(|(count, bytes)| (count, bytes / (1024 * 1024)))
            .unwrap_or((0, 0));
        let bmp_top = context
            .renderer
            .bitmap_texture_largest(15)
            .iter()
            .map(|(w, h, count, bytes)| format!("{w}x{h}x{count}={}MB", bytes / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(",");
        let (bmp_sizes, bmp_tracked) = context.renderer.bitmap_texture_buckets();
        let bmp_tracked_mb = bmp_tracked / (1024 * 1024);
        let (blend_cover, blend_cover_buckets) = context.renderer.take_blend_coverage();
        let blend_cover_hist = blend_cover_buckets
            .iter()
            .map(|count| count.to_string())
            .collect::<Vec<_>>()
            .join("/");

        let trivial_blend_modes = context
            .renderer
            .take_trivial_blend_counts()
            .iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(",");
        let blend_triv_top = take_live_trivial_blends(6).join(",");
        let (blend_triv_cover, blend_triv_mpx, blend_triv_alloc, blend_triv_buckets) =
            context.renderer.take_trivial_blend_target();
        let blend_triv_hist = blend_triv_buckets
            .iter()
            .map(|count| count.to_string())
            .collect::<Vec<_>>()
            .join("/");

        let line = format!(
            "orphans={orphans} loaders={loaders} cached_objects={cached_objects} \
             cache_mb={cache_mb} evicted_mb={evicted_mb} over_budget={over_budget} \
             movie_libs_total={movie_libs_total} movie_libs_aqw={movie_libs_aqw} \
             avatar_roots={avatar_roots} \
             redraw_large={redraw_large} redraw_small={redraw_small} \
             redraw_aged={redraw_aged} redraw_deferred={redraw_deferred} \
             defer_max={defer_max} starved={starved} \
             stale_fallback={stale_fallback} blend_layers={blend_layers} \
             tick_orphan_ms={tick_orphan_ms} tick_stage_ms={tick_stage_ms} \
             tick_bcast_ms={tick_bcast_ms} aqw_hook_ms={aqw_hook_ms} \
             aqw_clips={aqw_clips} aqw_playing={aqw_playing} \
             runframe_ms={runframe_ms} bcast_enter_ms={bcast_enter_ms} \
             listeners={listeners} listeners_susp={listeners_susp} \
             bcast_max={bcast_max}:{bcast_max_event} \
             goto_ms={goto_ms} goto_calls={goto_calls} goto_rewinds={goto_rewinds} \
             goto_frames={goto_frames} goto_scan_ms={goto_scan_ms} \
             goto_remove_ms={goto_remove_ms} goto_apply_ms={goto_apply_ms} \
             goto_place_ms={goto_place_ms} goto_inner_ms={goto_inner_ms} \
             goto_fast={goto_fast} goto_tags={goto_tags} bcast_top={bcast_top} \
             unqueue_ms={unqueue_ms} place_ms={place_ms} places={places} \
             list_copies={list_copies} list_copied={list_copied} \
             stage_enter_ms={stage_enter_ms} stage_ctor_ms={stage_ctor_ms} \
             stage_script_ms={stage_script_ms} orphans_frozen={orphans_frozen} \
             dirty_notex={dirty_notex} dirty_size={dirty_size} \
             dirty_matrix={dirty_matrix} dirty_invalid={dirty_invalid} \
             defer_grace={defer_grace} bypass_cleared={bypass_cleared} \
             orph_pop={orph_pop} orph_playing={orph_playing} orph_script={orph_script} \
             orph_avatar={orph_avatar} orph_quiet={orph_quiet} \
             invalidate_depth={invalidate_depth} dirty_origin={dirty_origin} \
             orph_probe_out={orph_probe_out} \
             orph_clean_visits={orph_clean_visits} orph_dirty_visits={orph_dirty_visits} \
             orph_clean_ms={orph_clean_ms} orph_dirty_ms={orph_dirty_ms}              script_rearm_skip={script_rearm_skip} timeline_stalls={timeline_stalls} \
             vram_mb={vram_mb} vram_budget_mb={vram_budget_mb} \
             vram_pressure={vram_pressure} \
             pool_allocs={pool_allocs} pool_free={pool_free} pool_mb={pool_mb} \
             tex_allocs={tex_allocs} tex_free={tex_free} tex_mb={tex_mb} \
             tex_top={tex_top} blend_modes={blend_modes} \
             blend_cover={blend_cover}% blend_cover_hist={blend_cover_hist} \
             blend_alloc={blend_alloc}% \
             blend_mul_opaque={blend_mul_opaque} \
             blend_triv_modes={trivial_blend_modes} \
             blend_triv_cover={blend_triv_cover}% blend_triv_hist={blend_triv_hist} \
             blend_triv_alloc={blend_triv_alloc}% \
             blend_triv_mpx={blend_triv_mpx} blend_triv_top={blend_triv_top} \
             render_encode_ms={render_encode_ms} render_submit_ms={render_submit_ms} \
             render_frames={render_frames} commit_mb={commit_mb} \
             blend_swfs={blend_swfs} avatar_caches={avatar_caches} \
             dicts={dicts} dict_okeys={dict_okeys} \
             bmp_tex={bmp_tex} bmp_mb={bmp_mb} bmp_top={bmp_top} \
             bmp_sizes={bmp_sizes} bmp_tracked_mb={bmp_tracked_mb} \
             inner_frames={inner_frames} inner_orphan_ms={inner_orphan_ms} \
             inner_stage_ms={inner_stage_ms} inner_bcast_ms={inner_bcast_ms} \
             inner_cleanup_ms={inner_cleanup_ms} \
             inner_orphan_visits={inner_orphan_visits} \
             inner_orphan_work={inner_orphan_work} gate_open={gate_open} \
             orphan_adds={orphan_adds} mark_bumps={mark_bumps} \
             blend_bake={blend_bake} blend_live={blend_live} \
             blend_bake_triv={blend_bake_triv} blend_live_triv={blend_live_triv} \
             bakes={bakes} bake_depth={bake_depth} \
             bake_reps={bake_reps} bake_reps_max={bake_reps_max} \
             bake_top={bake_top} blend_live_top={blend_live_top} \
             bypass_all={bypass_all} bypass_onscreen={bypass_onscreen} \
             bypass_saved={bypass_saved} \
             live_nocache={live_nocache} live_hadcache={live_hadcache} \
             live_cand={live_cand} cache_suppress={cache_suppress} \
             bypass_top={bypass_top}"
        );

        tracing::info!(target: "aqw_diag", "AQW sweep: {line}");

        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("aqw-memory.log"))
        {
            let _ = writeln!(file, "AQW sweep: {line}");
        }
    }
}

fn aqw_sweep_node<'gc>(
    obj: DisplayObject<'gc>,
    view_bounds: &Rectangle<Twips>,
    over_budget: bool,
    cached_objects: &mut u64,
    cache_bytes: &mut u64,
    evicted_bytes: &mut u64,
) {
    let object_cache_bytes = {
        let base = obj.base();
        let cache = base.bitmap_cache_mut();
        cache.as_ref().map_or(0, BitmapCache::estimated_bytes)
    };

    if object_cache_bytes > 0 {
        let on_screen = obj.world_bounds(BoundsMode::Engine).intersects(view_bounds);
        if over_budget && !on_screen {
            let base = obj.base();
            let mut cache_ref = base.bitmap_cache_mut();
            if let Some(cache) = cache_ref.as_mut() {
                cache.clear();
            }
            *evicted_bytes += object_cache_bytes;
        } else {
            *cached_objects += 1;
            *cache_bytes += object_cache_bytes;
        }
    }

    if let Some(container) = obj.as_container() {
        for child in container.iter_render_list() {
            aqw_sweep_node(
                child,
                view_bounds,
                over_budget,
                cached_objects,
                cache_bytes,
                evicted_bytes,
            );
        }
    }
}

#[derive(Default)]
struct OscillationState {
    prev1: f64,
    prev2: f64,
    flips: u32,
    reported: bool,
}

thread_local! {
    static OSCILLATION_PROBE:
        std::cell::RefCell<std::collections::HashMap<usize, OscillationState>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn note_position_oscillation<'gc>(this: DisplayObject<'gc>, context: &RenderContext<'_, 'gc>) {
    const REPORT_FLIPS: u32 = 30;
    const MAX_TRACKED: usize = 4096;

    let y = context.transform_stack.transform().matrix.ty.to_pixels();
    let key = this.as_ptr() as usize;

    let report = OSCILLATION_PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        if !probe.contains_key(&key) && probe.len() >= MAX_TRACKED {
            return None;
        }
        let state = probe.entry(key).or_default();
        if state.reported {
            return None;
        }

        let amplitude = (y - state.prev1).abs();
        let returned = (y - state.prev2).abs() < 0.01;
        if returned && (0.2..4.0).contains(&amplitude) {
            state.flips += 1;
        }
        state.prev2 = state.prev1;
        state.prev1 = y;

        if state.flips < REPORT_FLIPS {
            return None;
        }
        state.reported = true;
        Some(amplitude)
    });

    let Some(amplitude) = report else { return };

    let kind = match this {
        DisplayObject::MovieClip(_) => "MovieClip",
        DisplayObject::Bitmap(_) => "Bitmap",
        DisplayObject::Graphic(_) => "Graphic",
        DisplayObject::EditText(_) => "EditText",
        DisplayObject::LoaderDisplay(_) => "Loader",
        _ => "other",
    };
    let class = this
        .object2()
        .map(|o| {
            use crate::avm2::object::TObject;
            o.instance_class().name().local_name().to_string()
        })
        .unwrap_or_else(|| "<no avm2 object>".to_string());
    tracing::warn!(
        target: "aqw_diag",
        kind,
        class = class.as_str(),
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        movie = this.movie().url(),
        amplitude = format!("{amplitude:.4}"),
        between = format!("{:.4} / {:.4}", this.base().transform(true).matrix.ty.to_pixels(), y),
        depth = this.depth(),
        "AQW oscillation: drawn position alternating between two values"
    );
}

thread_local! {
    static ANIMATING_CLIPS: std::cell::RefCell<std::collections::HashMap<usize, (u16, u32, bool)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn note_map_clip_animation<'gc>(this: DisplayObject<'gc>) {
    const REPORT_CHANGES: u32 = 30;
    const MAX_REPORTS: usize = 25;

    let Some(clip) = this.as_movie_clip() else {
        return;
    };
    if !url_path_has_segment(clip.movie().url(), "maps") {
        return;
    }

    let frame = clip.current_frame();
    let key = this.as_ptr() as usize;
    let report = ANIMATING_CLIPS.with(|clips| {
        let mut clips = clips.borrow_mut();
        if clips.len() >= 512 && !clips.contains_key(&key) {
            return false;
        }
        let entry = clips.entry(key).or_insert((frame, 0, false));
        if entry.2 {
            return false;
        }
        if entry.0 != frame {
            entry.0 = frame;
            entry.1 += 1;
        }
        if entry.1 < REPORT_CHANGES {
            return false;
        }
        entry.2 = true;
        true
    });
    if !report {
        return;
    }

    static REPORTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }

    tracing::warn!(
        target: "aqw_diag",
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        class = this
            .object2()
            .map(|o| {
                use crate::avm2::object::TObject;
                o.instance_class().name().local_name().to_string()
            })
            .unwrap_or_else(|| "<none>".to_string()),
        frame,
        frames_loaded = clip.frames_loaded(),
        playing = clip.playing(),
        depth = this.depth(),
        movie = clip.movie().url(),
        "AQW map clip animating: timeline still advancing"
    );
}

thread_local! {
    static TINT_REPORTED: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

struct TintProgress {
    last_frame: u16,
    frame_changes: u32,
    last_color: ColorTransform,
    color_changes: u32,
    samples: u32,
}

thread_local! {
    static TINT_PROGRESS: std::cell::RefCell<std::collections::HashMap<usize, TintProgress>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static TINT_PROGRESS_DONE: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

fn note_tint_progression<'gc>(this: DisplayObject<'gc>, context: &RenderContext<'_, 'gc>) {
    const SAMPLES_BEFORE_REPORT: u32 = 16;
    const MAX_TRACKED: usize = 8192;
    const MAX_REPORTS: usize = 4000;

    let Some(clip) = this.as_movie_clip() else {
        return;
    };

    let url = clip.movie().url().to_owned();
    if !flicker_probe_covers(&url) {
        return;
    }

    let color = context.transform_stack.transform().color_transform;
    if color == Default::default() {
        return;
    }
    let own_color = this.base().color_transform();

    let frame = clip.current_frame();
    let key = this.as_ptr() as usize;
    if TINT_PROGRESS_DONE.with(|done| done.borrow().contains(&key)) {
        return;
    }
    let report = TINT_PROGRESS.with(|tracked| {
        let mut tracked = tracked.borrow_mut();
        if tracked.len() >= MAX_TRACKED && !tracked.contains_key(&key) {
            return None;
        }
        let entry = tracked.entry(key).or_insert(TintProgress {
            last_frame: frame,
            frame_changes: 0,
            last_color: color,
            color_changes: 0,
            samples: 0,
        });
        if entry.last_frame != frame {
            entry.last_frame = frame;
            entry.frame_changes += 1;
        }
        if entry.last_color != color {
            entry.last_color = color;
            entry.color_changes += 1;
        }
        entry.samples += 1;
        if entry.samples < SAMPLES_BEFORE_REPORT {
            return None;
        }
        let counts = (entry.frame_changes, entry.color_changes);
        tracked.remove(&key);
        TINT_PROGRESS_DONE.with(|done| done.borrow_mut().insert(key));
        Some(counts)
    });
    let Some((frame_changes, color_changes)) = report else {
        return;
    };

    static REPORTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }

    tracing::warn!(
        target: "aqw_diag",
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        depth = this.depth(),
        frame,
        frame_changes,
        color_changes,
        playing = clip.playing(),
        mult = format!(
            "{:.3},{:.3},{:.3}",
            color.r_multiply.to_f32(),
            color.g_multiply.to_f32(),
            color.b_multiply.to_f32()
        ),
        add = format!("{},{},{}", color.r_add, color.g_add, color.b_add),
        own = format!(
            "{:.3},{:.3},{:.3}/{},{},{}",
            own_color.r_multiply.to_f32(),
            own_color.g_multiply.to_f32(),
            own_color.b_multiply.to_f32(),
            own_color.r_add,
            own_color.g_add,
            own_color.b_add
        ),
        parent = this
            .parent()
            .and_then(|p| p.name().map(|n| n.to_string()))
            .unwrap_or_default(),
        movie = url.as_str(),
        "AQW tint progression over 48 samples"
    );
}

fn note_tint_mechanism<'gc>(this: DisplayObject<'gc>, context: &RenderContext<'_, 'gc>) {
    const MAX_REPORTS: usize = 4000;

    let url = this.movie().url().to_owned();
    if !flicker_probe_covers(&url) {
        return;
    }

    let blend = this.blend_mode();
    let filters = this.filters();
    let color = context.transform_stack.transform().color_transform;
    let tinted = color != Default::default();
    if blend == ExtendedBlendMode::Normal && filters.is_empty() && !tinted {
        return;
    }

    let key = this.as_ptr() as usize;
    let fresh = TINT_REPORTED.with(|seen| {
        let mut seen = seen.borrow_mut();
        seen.len() < MAX_REPORTS && seen.insert(key)
    });
    if !fresh {
        return;
    }

    let filter_names = filters
        .iter()
        .map(|f| {
            format!("{f:?}")
                .split('(')
                .next()
                .unwrap_or("?")
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("+");
    tracing::warn!(
        target: "aqw_diag",
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        depth = this.depth(),
        blend = ?blend,
        filters = filter_names.as_str(),
        mult = format!(
            "{:.3},{:.3},{:.3},{:.3}",
            color.r_multiply.to_f32(),
            color.g_multiply.to_f32(),
            color.b_multiply.to_f32(),
            color.a_multiply.to_f32()
        ),
        add = format!(
            "{},{},{},{}",
            color.r_add, color.g_add, color.b_add, color.a_add
        ),
        movie = this.movie().url(),
        "AQW tint: how this object is coloured"
    );
}

thread_local! {
    static CACHE_DECISION_STATE:
        std::cell::RefCell<std::collections::HashMap<usize, (u64, u32)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn coarse_colour(ct: &ColorTransform) -> (i8, i8, i8, i8, i8, i8) {
    let step = |m: f32| (m * 8.0).round().clamp(-128.0, 127.0) as i8;
    let add = |a: i16| (a / 32) as i8;
    (
        step(ct.r_multiply.to_f32()),
        step(ct.g_multiply.to_f32()),
        step(ct.b_multiply.to_f32()),
        add(ct.r_add),
        add(ct.g_add),
        add(ct.b_add),
    )
}

fn ancestor_names<'gc>(this: DisplayObject<'gc>, levels: usize) -> String {
    let mut names = Vec::with_capacity(levels);
    let mut node = this.parent();
    while let Some(parent) = node {
        if names.len() >= levels {
            break;
        }
        names.push(
            parent
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
        node = parent.parent();
    }
    names.join("<")
}

fn note_colour_write<'gc>(this: DisplayObject<'gc>, new: &ColorTransform) {
    const MAX_REPORTS: usize = 4000;

    let movie = this.movie();
    if !flicker_probe_covers(movie.url()) {
        return;
    }
    static REPORTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }

    let class = this
        .object2()
        .map(|o| {
            use crate::avm2::object::TObject;
            o.instance_class().name().local_name().to_string()
        })
        .unwrap_or_else(|| "-".to_string());
    tracing::warn!(
        target: "aqw_diag",
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        class = class.as_str(),
        depth = this.depth(),
        holders = ancestor_names(this, 4).as_str(),
        mult = format!(
            "{:.3},{:.3},{:.3}",
            new.r_multiply.to_f32(),
            new.g_multiply.to_f32(),
            new.b_multiply.to_f32()
        ),
        add = format!("{},{},{}", new.r_add, new.g_add, new.b_add),
        movie = movie.url(),
        "AQW colour written to this object"
    );
}

fn note_cache_decision<'gc>(
    this: DisplayObject<'gc>,
    context: &RenderContext<'_, 'gc>,
    cache_info: Option<&DrawCacheInfo>,
) {
    const MAX_REPORTS_PER_OBJECT: u32 = 80;
    const MAX_TRACKED: usize = 40000;

    let movie = this.movie();
    if !flicker_probe_covers(movie.url()) {
        return;
    }

    let matrix = context.transform_stack.transform().matrix;
    let scale = f64::from(matrix.a.abs().max(matrix.d.abs()));
    if !scale.is_finite() {
        return;
    }

    let outcome = match cache_info {
        Some(info) if info.dirty => "cache-baked-now",
        Some(_) => "cache-composited",
        None => "vectors",
    };
    let mut has_texture = false;
    let mut cache_size = (0u32, 0u32);
    let mut has_cache = false;
    if let Some(cache) = &*this.base().bitmap_cache_mut() {
        has_cache = true;
        has_texture = cache.bitmap.is_some();
        cache_size = (cache.source_width, cache.source_height);
    }
    let composed = context.transform_stack.transform().color_transform;
    let own = this.base().color_transform();

    let bucket = (scale * 8.0).round() as i32;
    let state = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (
            outcome,
            has_texture,
            has_cache,
            cache_size,
            bucket,
            context.is_offscreen,
            coarse_colour(&composed),
            coarse_colour(&own),
        )
            .hash(&mut hasher);
        hasher.finish()
    };
    let changed = CACHE_DECISION_STATE.with(|tracked| {
        let mut tracked = tracked.borrow_mut();
        let key = this.as_ptr() as usize;
        if tracked.len() >= MAX_TRACKED && !tracked.contains_key(&key) {
            return false;
        }
        match tracked.get_mut(&key) {
            Some((last, spent)) => {
                if *last == state || *spent >= MAX_REPORTS_PER_OBJECT {
                    return false;
                }
                *last = state;
                *spent += 1;
                true
            }
            None => {
                tracked.insert(key, (state, 1));
                true
            }
        }
    });
    if !changed {
        return;
    }

    let texture_size = if has_cache {
        format!("{}x{}", cache_size.0, cache_size.1)
    } else {
        String::from("-")
    };

    let view = context.stage.view_matrix();
    let view_scale = f64::from(view.a.abs().max(view.d.abs()));
    let class = this
        .object2()
        .map(|o| {
            use crate::avm2::object::TObject;
            o.instance_class().name().local_name().to_string()
        })
        .unwrap_or_else(|| "-".to_string());

    tracing::warn!(
        target: "aqw_diag",
        outcome,
        offscreen = context.is_offscreen,
        wants_cache = this.is_bitmap_cached(),
        has_texture,
        tex = texture_size.as_str(),
        scale = format!("{scale:.3}"),
        view_scale = format!("{view_scale:.3}"),
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        class = class.as_str(),
        depth = this.depth(),
        holders = ancestor_names(this, 4).as_str(),
        mult = format!(
            "{:.3},{:.3},{:.3}",
            composed.r_multiply.to_f32(),
            composed.g_multiply.to_f32(),
            composed.b_multiply.to_f32()
        ),
        add = format!("{},{},{}", composed.r_add, composed.g_add, composed.b_add),
        own = format!(
            "{:.3},{:.3},{:.3}/{},{},{}",
            own.r_multiply.to_f32(),
            own.g_multiply.to_f32(),
            own.b_multiply.to_f32(),
            own.r_add,
            own.g_add,
            own.b_add
        ),
        movie = movie.url(),
        "AQW cache decision at this camera distance"
    );
}

pub fn render_base<'gc>(
    this: DisplayObject<'gc>,
    context: &mut RenderContext<'_, 'gc>,
    options: RenderOptions,
) {
    if options.skip_masks && this.maskee().is_some() {
        // Skip rendering masks (unless we are rendering one explicitly).
        return;
    }

    let Some(_render_guard) =
        DisplayObjectRecursionGuard::enter(&RENDER_RECURSION_DEPTH, "render", this)
    else {
        return;
    };

    if render_aqw_scaling_grid(this, context, options) {
        return;
    }

    if options.apply_transform {
        let transform = this.base().transform(options.apply_matrix);
        context.transform_stack.push(&transform);
        if aqw_flicker_probe_enabled() {
            note_position_oscillation(this, context);
            note_map_clip_animation(this);
            note_tint_mechanism(this, context);
            note_tint_progression(this, context);
        }
    }

    let blend_mode = this.blend_mode();
    let blend_bake_owner = if blend_mode != ExtendedBlendMode::Normal {
        aqw_current_bake()
    } else {
        None
    };
    let original_commands = if blend_mode != ExtendedBlendMode::Normal {
        AQW_BLEND_LAYERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if aqw_diagnostics_enabled() {
            note_blend_source(this.movie().url());
        }
        Some(std::mem::take(&mut context.commands))
    } else {
        None
    };

    let nested_filter_cache = !context.use_bitmap_cache
        && context.cache_filtered_children
        && !aqw_nested_filter_cache_disabled()
        && !this.filters().is_empty();
    let mut suppress_reason: Option<CacheSuppressReason> = None;
    let mut suppress_detail: Option<String> = None;
    let cache_info = if (context.use_bitmap_cache || nested_filter_cache) && this.is_bitmap_cached()
    {
        let mut cache_info: Option<DrawCacheInfo> = None;
        let base_transform = context.transform_stack.transform();
        let allow_aqw_large_cache = is_aqw_movie_url(this.movie().url());
        let aqw_auto_cache = !aqw_avatar_cache_disabled()
            && this
                .as_movie_clip()
                .is_some_and(|clip| clip.is_aqw_avatar_asset_root());
        let m = &base_transform.matrix;
        let degenerate_transform = allow_aqw_large_cache
            && (!m.a.is_finite()
                || !m.b.is_finite()
                || !m.c.is_finite()
                || !m.d.is_finite()
                || m.a.abs().max(m.b.abs()).max(m.c.abs()).max(m.d.abs()) > CACHE_DEGENERATE_SCALE);
        let bounds: Rectangle<Twips> = if degenerate_transform {
            Rectangle {
                x_min: Twips::ZERO,
                x_max: Twips::ZERO,
                y_min: Twips::ZERO,
                y_max: Twips::ZERO,
            }
        } else {
            this.render_bounds_with_transform(
                &base_transform.matrix,
                false, // we do the filter growth for this object ourselves, to know the offsets
                &context.stage.view_matrix(),
            )
        };
        let name = this.name();
        let mut filters: Vec<Filter> = this.filters().to_owned();
        let swf_version = this.swf_version();
        filters.retain(|f| !f.impotent());
        let bypass_bitmap_cache = degenerate_transform
            || should_bypass_offscreen_bitmap_cache(this, context, options, &bounds, &filters);
        let allow_size_padding = allow_aqw_large_cache
            && !aqw_padded_cache_disabled()
            && this.opaque_background().is_none();

        if let Some(cache) = &mut *this.base().bitmap_cache_mut() {
            if bypass_bitmap_cache {
                if aqw_diagnostics_enabled() && cache.bitmap.is_some() {
                    AQW_CACHE_BYPASS_CLEARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if aqw_diagnostics_enabled() {
                    note_cache_bypass(this, context, &bounds);
                }
                cache.clear();
                suppress_reason = Some(CacheSuppressReason::Bypassed);
            } else {
                let width = bounds.width().to_pixels().ceil().max(0.0);
                let height = bounds.height().to_pixels().ceil().max(0.0);
                if width <= u16::MAX as f64 && height <= u16::MAX as f64 {
                    let width = width as u32;
                    let height = height as u32;
                    let mut filter_rect = Rectangle {
                        x_min: Twips::ZERO,
                        x_max: Twips::from_pixels_i32(width as i32),
                        y_min: Twips::ZERO,
                        y_max: Twips::from_pixels_i32(height as i32),
                    };
                    let stage_matrix = context.stage.view_matrix();
                    for filter in &mut filters {
                        filter.scale(stage_matrix.a, stage_matrix.d);
                        filter_rect = filter.calculate_dest_rect(filter_rect);
                    }
                    let filter_rect = Rectangle {
                        x_min: filter_rect.x_min.to_pixels().floor() as i32,
                        x_max: filter_rect.x_max.to_pixels().ceil() as i32,
                        y_min: filter_rect.y_min.to_pixels().floor() as i32,
                        y_max: filter_rect.y_max.to_pixels().ceil() as i32,
                    };
                    let draw_offset = Point::new(filter_rect.x_min, filter_rect.y_min);
                    let actual_width = filter_rect.width().max(0) as u32;
                    let actual_height = filter_rect.height().max(0) as u32;
                    if cache.is_dirty(&base_transform.matrix, width, height) {
                        if aqw_diagnostics_enabled() {
                            use std::sync::atomic::Ordering::Relaxed;
                            match cache.dirty_reason(&base_transform.matrix, width, height) {
                                Some(CacheDirtyReason::NoTexture) => {
                                    AQW_CACHE_DIRTY_NOTEX.fetch_add(1, Relaxed)
                                }
                                Some(CacheDirtyReason::Size) => {
                                    AQW_CACHE_DIRTY_SIZE.fetch_add(1, Relaxed)
                                }
                                Some(CacheDirtyReason::Invalidated) => {
                                    AQW_CACHE_DIRTY_INVALID.fetch_add(1, Relaxed)
                                }
                                Some(CacheDirtyReason::Matrix) => {
                                    AQW_CACHE_DIRTY_MATRIX.fetch_add(1, Relaxed)
                                }
                                None => 0,
                            };
                        }
                        cache.dirty_streak = cache.dirty_streak.saturating_add(1);
                        let redraw_pixels = u64::from(actual_width) * u64::from(actual_height);
                        let view_scale = {
                            let view = context.stage.view_matrix();
                            f64::from(view.a.abs().max(view.d.abs())).max(1.0)
                        };
                        let defer_min_pixels = (AQW_DIRTY_CACHE_REDRAW_DEFER_MIN_PIXELS as f64
                            * view_scale
                            * view_scale) as u64;
                        let defer_min_side =
                            (f64::from(AQW_DIRTY_CACHE_REDRAW_DEFER_MIN_SIDE) * view_scale) as u32;
                        let is_large = redraw_pixels >= defer_min_pixels
                            && (actual_width >= defer_min_side || actual_height >= defer_min_side);
                        let mut admitted_aged = false;
                        let grace_admitted = cache.dirty_streak <= aqw_defer_eligible_frames();
                        let mut can_redraw_cache = if !allow_aqw_large_cache || aqw_auto_cache {
                            true
                        } else if grace_admitted {
                            AQW_CACHE_DEFER_GRACE
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            true
                        } else if is_large {
                            context.try_reserve_dirty_cache_redraw(redraw_pixels)
                        } else {
                            context.try_reserve_small_cache_redraw(redraw_pixels)
                        };
                        if !can_redraw_cache
                            && cache.deferred_frames >= aqw_stale_cache_aged_frames()
                            && context.try_reserve_aged_cache_redraw()
                        {
                            can_redraw_cache = true;
                            admitted_aged = true;
                        }
                        if !can_redraw_cache
                            && cache.deferred_frames >= aqw_defer_max_stale_frames()
                        {
                            can_redraw_cache = true;
                        }

                        if can_redraw_cache {
                            cache.bake_count = cache.bake_count.saturating_add(1);
                            if aqw_diagnostics_enabled() {
                                aqw_note_bake_repeat(cache.bake_count);
                            }
                            if aqw_diagnostics_enabled()
                                && let Some(streak) = cache.note_static_churn(
                                    &base_transform.matrix,
                                    width,
                                    height,
                                    draw_offset,
                                )
                            {
                                let was = (cache.source_width, cache.source_height);
                                let was_offset = cache.draw_offset;
                                let class = this
                                    .object2()
                                    .map(|o| {
                                        use crate::avm2::object::TObject;
                                        o.instance_class().name().local_name().to_string()
                                    })
                                    .unwrap_or_else(|| "<no avm2 object>".to_string());
                                tracing::warn!(
                                    target: "aqw_diag",
                                    streak,
                                    class = class.as_str(),
                                    movie = this.movie().url(),
                                    was_size = format!("{}x{}", was.0, was.1),
                                    now_size = format!("{width}x{height}"),
                                    was_offset = format!("{},{}", was_offset.x, was_offset.y),
                                    now_offset = format!("{},{}", draw_offset.x, draw_offset.y),
                                    filters = filters.len(),
                                    "AQW cache churn: geometry moving under a static transform"
                                );
                            }
                            cache.update(
                                context.renderer,
                                base_transform.matrix,
                                width,
                                height,
                                actual_width,
                                actual_height,
                                draw_offset,
                                swf_version,
                                allow_aqw_large_cache,
                                allow_size_padding,
                            );
                            cache.deferred_frames = 0;
                            cache.stale_anchor = Point::new(
                                bounds.x_min - base_transform.matrix.tx
                                    + Twips::from_pixels_i32(draw_offset.x),
                                bounds.y_min - base_transform.matrix.ty
                                    + Twips::from_pixels_i32(draw_offset.y),
                            );
                            if allow_aqw_large_cache {
                                use std::sync::atomic::Ordering;
                                if admitted_aged {
                                    AQW_CACHE_REDRAWS_AGED.fetch_add(1, Ordering::Relaxed);
                                } else if is_large {
                                    AQW_CACHE_REDRAWS_LARGE.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    AQW_CACHE_REDRAWS_SMALL.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            let probe = (base_transform.matrix, bounds, filters.len());
                            cache_info = cache.handle().map(|handle| DrawCacheInfo {
                                handle,
                                dirty: true,
                                base_transform,
                                bounds,
                                draw_offset,
                                filters,
                                offset_override: None,
                                aqw_auto_cache,
                            });
                            if cache_info.is_none() {
                                suppress_reason =
                                    Some(if actual_width == 0 || actual_height == 0 {
                                        suppress_detail = Some(format!(
                                            "{width}x{height}->{actual_width}x{actual_height}"
                                        ));
                                        if aqw_diagnostics_enabled() && !cache.warned_for_zero {
                                            cache.warned_for_zero = true;
                                            let (m, b, filter_count) = probe;
                                            tracing::warn!(
                                                target: "aqw_diag",
                                                movie = this.movie().url(),
                                                x_min = b.x_min.get(),
                                                x_max = b.x_max.get(),
                                                y_min = b.y_min.get(),
                                                y_max = b.y_max.get(),
                                                m_a = m.a,
                                                m_b = m.b,
                                                m_c = m.c,
                                                m_d = m.d,
                                                m_tx = m.tx.get(),
                                                m_ty = m.ty.get(),
                                                filters = filter_count,
                                                "AQW cache resolved to a zero-sided rect"
                                            );
                                        }
                                        CacheSuppressReason::AllocZero
                                    } else {
                                        CacheSuppressReason::AllocRefused
                                    });
                            }
                        } else {
                            cache.deferred_frames = cache.deferred_frames.saturating_add(1);
                            AQW_CACHE_REDRAWS_DEFERRED
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if aqw_diagnostics_enabled() {
                                AQW_CACHE_DEFER_MAX.fetch_max(
                                    cache.deferred_frames as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                if cache.deferred_frames >= aqw_stale_cache_aged_frames() {
                                    AQW_CACHE_STARVED
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            let current_offset = Point::new(
                                bounds.x_min - base_transform.matrix.tx
                                    + Twips::from_pixels_i32(draw_offset.x),
                                bounds.y_min - base_transform.matrix.ty
                                    + Twips::from_pixels_i32(draw_offset.y),
                            );
                            let max_drift_twips = if aqw_drift_norm_disabled() {
                                AQW_STALE_ANCHOR_MAX_DRIFT_TWIPS
                            } else {
                                (f64::from(AQW_STALE_ANCHOR_MAX_DRIFT_TWIPS) * view_scale) as i32
                            };
                            let anchor_drifted = !aqw_stale_anchor_disabled()
                                && !aqw_stale_guard_disabled()
                                && ((cache.stale_anchor.x - current_offset.x).get().abs()
                                    > max_drift_twips
                                    || (cache.stale_anchor.y - current_offset.y).get().abs()
                                        > max_drift_twips);
                            let offset_override =
                                (!aqw_stale_anchor_disabled()).then_some(cache.stale_anchor);
                            cache_info = if anchor_drifted {
                                AQW_CACHE_STALE_FALLBACKS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                suppress_reason = Some(CacheSuppressReason::DriftFallback);
                                None
                            } else {
                                cache.handle().map(|handle| DrawCacheInfo {
                                    handle,
                                    dirty: false,
                                    base_transform,
                                    bounds,
                                    draw_offset,
                                    filters,
                                    offset_override,
                                    aqw_auto_cache,
                                })
                            };

                            if cache_info.is_none() && suppress_reason.is_none() {
                                suppress_reason = Some(CacheSuppressReason::DeferredNoTexture);
                            }
                            if aqw_diagnostics_enabled() {
                                tracing::info!(
                                    target: "aqw_diag",
                                    width,
                                    height,
                                    actual_width,
                                    actual_height,
                                    redraw_pixels,
                                    has_stale_cache = cache_info.is_some(),
                                    "Deferring dirty AQW bitmap cache redraw"
                                );
                            }
                        }
                    } else {
                        cache.deferred_frames = 0;
                        cache.dirty_streak = 0;
                        if cache.bitmap.is_none() {
                            suppress_reason = Some(CacheSuppressReason::CleanNoTexture);
                        }
                        cache_info = cache.handle().map(|handle| DrawCacheInfo {
                            handle,
                            dirty: false,
                            base_transform,
                            bounds,
                            draw_offset,
                            filters,
                            offset_override: None,
                            aqw_auto_cache,
                        });
                    }
                } else {
                    suppress_reason = Some(CacheSuppressReason::OversizeU16);
                    if !cache.warned_for_oversize {
                        tracing::warn!(
                            "Skipping cacheAsBitmap for incredibly large object {:?} ({width} x {height})",
                            name
                        );
                        if aqw_diagnostics_enabled() {
                            let m = &base_transform.matrix;
                            tracing::warn!(
                                target: "aqw_diag",
                                movie = this.movie().url(),
                                x_min = bounds.x_min.get(),
                                x_max = bounds.x_max.get(),
                                y_min = bounds.y_min.get(),
                                y_max = bounds.y_max.get(),
                                m_a = m.a,
                                m_b = m.b,
                                m_c = m.c,
                                m_d = m.d,
                                "AQW cache resolved to an enormous rect"
                            );
                        }
                        cache.warned_for_oversize = true;
                    }
                    cache.clear();
                    cache_info = None;
                }
            }
        }
        cache_info
    } else {
        if this.is_bitmap_cached() {
            suppress_reason = Some(CacheSuppressReason::NotHere);
        }
        None
    };

    if aqw_diagnostics_enabled()
        && let Some(reason) = suppress_reason
    {
        note_cache_suppressed(this, reason, suppress_detail.as_deref());
    }

    if aqw_flicker_probe_enabled() {
        note_cache_decision(this, context, cache_info.as_ref());
    }

    // We can't hold `cache` (which will hold `base`), so this is split up
    if let Some(cache_info) = cache_info {
        // In order to render an object to a texture, we need to draw its entire bounds.
        // Calculate the offset from tx/ty in order to accommodate any drawings that extend the bounds
        let (offset_x, offset_y) = if let Some(anchor) = cache_info.offset_override {
            (anchor.x, anchor.y)
        } else {
            (
                cache_info.bounds.x_min - cache_info.base_transform.matrix.tx
                    + Twips::from_pixels_i32(cache_info.draw_offset.x),
                cache_info.bounds.y_min - cache_info.base_transform.matrix.ty
                    + Twips::from_pixels_i32(cache_info.draw_offset.y),
            )
        };

        if cache_info.dirty {
            let mut transform_stack = TransformStack::new();
            transform_stack.push(&Transform {
                color_transform: Default::default(),
                matrix: Matrix {
                    tx: -offset_x,
                    ty: -offset_y,
                    ..cache_info.base_transform.matrix
                },
                perspective_projection: cache_info.base_transform.perspective_projection,
            });
            let mut offscreen_context = RenderContext {
                renderer: context.renderer,
                commands: CommandList::new(),
                cache_draws: context.cache_draws,
                gc_context: context.gc_context,
                library: context.library,
                transform_stack: &mut transform_stack,
                is_offscreen: true,
                use_bitmap_cache: !is_aqw_movie_url(this.movie().url()),
                cache_filtered_children: true,
                dirty_cache_redraws_remaining: u32::MAX,
                dirty_cache_redraws_reserved: 0,
                dirty_cache_redraw_pixels_remaining: u64::MAX,
                small_cache_redraws_remaining: u32::MAX,
                small_cache_redraws_reserved: 0,
                small_cache_redraw_pixels_remaining: u64::MAX,
                aged_cache_redraws_remaining: u32::MAX,
                stage: context.stage,
            };
            let _bake_scope = AqwBakeScope::enter(this);
            this.render_self(&mut offscreen_context);
            offscreen_context.cache_draws.push(BitmapCacheEntry {
                handle: cache_info.handle.clone(),
                commands: offscreen_context.commands,
                clear: this.opaque_background().unwrap_or_default(),
                filters: cache_info.filters,
            });
        }

        // When rendering it back, ensure we're only keeping the translation - scale/rotation is within the image already
        // Snapping a cache to whole pixels is what Flash does, and it is right for a cache the
        // content asked for.
        let pixel_snapping = if cache_info.aqw_auto_cache {
            PixelSnapping::Never
        } else {
            PixelSnapping::Always
        };
        apply_standard_mask_and_scroll(
            this,
            context,
            |context| {
                context.commands.render_bitmap(
                    cache_info.handle,
                    Transform {
                        matrix: Matrix {
                            tx: context.transform_stack.transform().matrix.tx + offset_x,
                            ty: context.transform_stack.transform().matrix.ty + offset_y,
                            ..Default::default()
                        },
                        color_transform: cache_info.base_transform.color_transform,
                        perspective_projection: cache_info.base_transform.perspective_projection,
                    },
                    true,
                    pixel_snapping,
                )
            },
            options,
        );
    } else {
        if let Some(background) = this.opaque_background() {
            // This is intended for use with cacheAsBitmap, but can be set for non-cached objects too
            // It wants the entire bounding box to be cleared before any draws happen
            let bounds: Rectangle<Twips> = this.render_bounds_with_transform(
                &context.transform_stack.transform().matrix,
                true,
                &context.stage.view_matrix(),
            );
            context
                .commands
                .draw_rect(background, Matrix::create_box_from_rectangle(&bounds));
        }
        apply_standard_mask_and_scroll(this, context, |context| this.render_self(context), options);
    }

    if let Some(original_commands) = original_commands {
        let sub_commands = std::mem::replace(&mut context.commands, original_commands);
        // If there's nothing to draw, throw away the blend entirely.
        if !sub_commands.is_empty() {
            if aqw_diagnostics_enabled() {
                note_blend_emitted(
                    this,
                    blend_bake_owner.as_deref(),
                    is_complex_blend(blend_mode),
                );
            }
            let render_blend_mode = if let ExtendedBlendMode::Shader = blend_mode {
                // Note - Flash appears to let you set blend mode to shader
                // without having blend shader set.  In this case, Flash seems
                // to fall back to a normal blend.
                if let Some(shader) = this.blend_shader() {
                    RenderBlendMode::Shader(shader)
                } else {
                    RenderBlendMode::Builtin(swf::BlendMode::Normal)
                }
            } else {
                RenderBlendMode::Builtin(blend_mode.try_into().unwrap())
            };
            context.commands.blend(sub_commands, render_blend_mode);
        }
    }

    if options.apply_transform {
        context.transform_stack.pop();
    }
}

/// This applies the **standard** method of `mask` and `scrollRect`.
///
/// It uses the stencil buffer so that any pixel drawn in the mask will allow the inner contents to show.
/// This is what is used for most cases, except for cacheAsBitmap-on-cacheAsBitmap.
pub fn apply_standard_mask_and_scroll<'gc, F>(
    this: DisplayObject<'gc>,
    context: &mut RenderContext<'_, 'gc>,
    draw: F,
    options: RenderOptions,
) where
    F: FnOnce(&mut RenderContext<'_, 'gc>),
{
    let scroll_rect_matrix = if let Some(rect) = this.scroll_rect() {
        let cur_transform = context.transform_stack.transform();
        // The matrix we use for actually drawing a rectangle for cropping purposes
        // Note that we do *not* apply the translation yet
        Some(
            cur_transform.matrix
                * Matrix::scale(
                    rect.width().to_pixels() as f32,
                    rect.height().to_pixels() as f32,
                ),
        )
    } else {
        None
    };

    if let Some(rect) = this.scroll_rect() {
        // Translate everything that we render (including DisplayObject.mask)
        context.transform_stack.push(&Transform {
            matrix: Matrix::translate(-rect.x_min, -rect.y_min),
            color_transform: Default::default(),
            perspective_projection: None,
        });
    }

    let mask = this.get_render_mask();
    let mut mask_transform = ruffle_render::transform::Transform::default();
    if let RenderMask::Stencil(m) | RenderMask::Alpha(m) = mask {
        if options.apply_transform {
            mask_transform.matrix = this.global_to_local_matrix().unwrap_or_default();
        }
        mask_transform.matrix *= m.local_to_global_matrix();
    }
    if let RenderMask::Stencil(m) = mask {
        context.commands.push_mask();
        context.transform_stack.push(&mask_transform);
        if let Some(_render_guard) = DisplayObjectRecursionGuard::enter(
            &RENDER_RECURSION_DEPTH,
            "stencil_mask_render_self",
            m,
        ) {
            m.render_self(context);
        }
        context.transform_stack.pop();
        context.commands.activate_mask();
    }

    // There are two parts to 'DisplayObject.scrollRect':
    // a scroll effect (translation), and a crop effect.
    // This scroll is implementing by applying a translation matrix
    // when we defined 'scroll_rect_matrix'.
    // The crop is implemented as a rectangular mask using the height
    // and width provided by 'scrollRect'.

    // Note that this mask is applied *in addition to* a mask defined
    // with 'DisplayObject.mask'. We will end up rendering content that
    // lies in the intersection of the scroll rect and DisplayObject.mask,
    // which is exactly the behavior that we want.
    if let Some(rect_mat) = scroll_rect_matrix {
        context.commands.push_mask();
        // The color doesn't matter, as this is a mask.
        context.commands.draw_rect(Color::WHITE, rect_mat);
        context.commands.activate_mask();
    }

    if let RenderMask::Alpha(m) = mask {
        let original_commands = std::mem::take(&mut context.commands);

        draw(context);

        let maskee_commands = std::mem::take(&mut context.commands);

        context.transform_stack.push(&mask_transform);
        let options = RenderOptions {
            skip_masks: false,
            apply_matrix: false,
            ..Default::default()
        };
        m.render_with_options(context, options);
        context.transform_stack.pop();

        let mask_commands = std::mem::replace(&mut context.commands, original_commands);

        context
            .commands
            .render_alpha_mask(maskee_commands, mask_commands);
    } else {
        draw(context);
    }

    if let Some(rect_mat) = scroll_rect_matrix {
        // Draw the rectangle again after deactivating the mask,
        // to reset the stencil buffer.
        context.commands.deactivate_mask();
        context.commands.draw_rect(Color::WHITE, rect_mat);
        context.commands.pop_mask();
    }

    if let RenderMask::Stencil(m) = mask {
        context.commands.deactivate_mask();
        context.transform_stack.push(&mask_transform);
        if let Some(_render_guard) = DisplayObjectRecursionGuard::enter(
            &RENDER_RECURSION_DEPTH,
            "stencil_mask_clear_render_self",
            m,
        ) {
            m.render_self(context);
        }
        context.transform_stack.pop();
        context.commands.pop_mask();
    }

    if scroll_rect_matrix.is_some() {
        // Remove the translation that we pushed
        context.transform_stack.pop();
    }
}

#[enum_trait_object(
    #[derive(Clone, Collect, Debug, Copy)]
    #[collect(no_drop)]
    pub enum DisplayObject<'gc> {
        Stage(Stage<'gc>),
        Bitmap(Bitmap<'gc>),
        Avm1Button(Avm1Button<'gc>),
        Avm2Button(Avm2Button<'gc>),
        EditText(EditText<'gc>),
        TextLine(TextLine<'gc>),
        Graphic(Graphic<'gc>),
        MorphShape(MorphShape<'gc>),
        MovieClip(MovieClip<'gc>),
        Text(Text<'gc>),
        Video(Video<'gc>),
        LoaderDisplay(LoaderDisplay<'gc>)
    }
)]
pub trait TDisplayObject<'gc>:
    'gc + Clone + Copy + Collect<'gc> + Debug + Into<DisplayObject<'gc>>
{
    fn base(self) -> Gc<'gc, DisplayObjectBase<'gc>>;

    #[no_dynamic]
    fn as_ptr(self) -> *const DisplayObjectPtr {
        Gc::as_ptr(self.base()).cast()
    }

    /// The `SCALE_ROTATION_CACHED` flag should only be set in SWFv5+.
    /// So scaling/rotation values always have to get recalculated from the matrix in SWFv4.
    /// SWF version 0 means non-SWF content (a loaded image); since loading images requires
    /// `loadMovie` (SWFv5+) or `MovieClipLoader` (SWFv6+), this can't occur in a SWFv4 context.
    /// Therefore, loaded images are supposed to work the way SWF >= 5 movies do in this regard,
    /// but the SWF version of the MovieClips created for loaded images can't inherit their
    /// version from the loading movie - they have to be reported as -1 to ActionScript.
    #[no_dynamic]
    fn set_scale_rotation_cached(self) {
        if self.swf_version() == 0 || self.swf_version() >= 5 {
            self.base().set_scale_rotation_cached(true);
        }
    }

    fn id(self) -> CharacterId;

    #[no_dynamic]
    fn depth(self) -> Depth {
        self.base().depth()
    }

    #[no_dynamic]
    fn set_depth(self, depth: Depth) {
        self.base().set_depth(depth)
    }

    /// The untransformed inherent bounding box of this object.
    /// These bounds do **not** include child DisplayObjects.
    /// To get the bounds including children, use `bounds`, `local_bounds`, or `world_bounds`.
    ///
    /// The `mode` parameter indicates which kind of bounds to return:
    /// - `BoundsMode::Engine`: Actual visual bounds (for hit testing, rendering)
    /// - `BoundsMode::Script`: Bounds as reported by ActionScript (some objects like MorphShape
    ///   always return the start shape's bounds)
    ///
    /// Implementors must override this method.
    /// Leaf DisplayObjects should return their bounds.
    /// Composite DisplayObjects that only contain children should return `Default::default()`
    fn self_bounds(self, mode: BoundsMode) -> Rectangle<Twips>;

    /// The untransformed bounding box of this object including children.
    #[no_dynamic]
    fn bounds(self, mode: BoundsMode) -> Rectangle<Twips> {
        self.bounds_with_transform(&Matrix::default(), mode)
    }

    /// The local bounding box of this object including children, in its parent's coordinate system.
    #[no_dynamic]
    fn local_bounds(self, mode: BoundsMode) -> Rectangle<Twips> {
        self.bounds_with_transform(&self.base().matrix(), mode)
    }

    /// The world bounding box of this object including children, relative to the stage.
    #[no_dynamic]
    fn world_bounds(self, mode: BoundsMode) -> Rectangle<Twips> {
        self.bounds_with_transform(&self.local_to_global_matrix(), mode)
    }

    /// The world bounding box of this object, as reported by `Transform.pixelBounds`.
    fn pixel_bounds(self, mode: BoundsMode) -> Rectangle<Twips> {
        self.world_bounds(mode)
    }

    /// Bounds used for drawing debug rects and picking objects.
    #[no_dynamic]
    fn debug_rect_bounds(self) -> Rectangle<Twips> {
        // Make the rect at least as big as highlight bounds to ensure that anything
        // interactive is also highlighted even if not included in world bounds.
        let highlight_bounds = self
            .as_interactive()
            .map(|int| int.highlight_bounds())
            .unwrap_or_default();
        self.world_bounds(BoundsMode::Engine)
            .union(&highlight_bounds)
    }

    /// Gets the bounds of this object and all children, transformed by a given matrix.
    /// This function recurses down and transforms the AABB each child before adding
    /// it to the bounding box. This gives a tighter AABB then if we simply transformed
    /// the overall AABB.
    ///
    /// The `mode` parameter indicates which kind of bounds to return.
    fn bounds_with_transform(self, matrix: &Matrix, mode: BoundsMode) -> Rectangle<Twips> {
        let Some(_bounds_guard) =
            DisplayObjectRecursionGuard::enter(&BOUNDS_RECURSION_DEPTH, "bounds", self.into())
        else {
            return *matrix * self.self_bounds(mode);
        };

        // A scroll rect completely overrides an object's bounds,
        // and can even grow the bounding box to be larger than the actual content
        if let Some(scroll_rect) = self.scroll_rect() {
            return *matrix
                * Rectangle {
                    x_min: Twips::ZERO,
                    y_min: Twips::ZERO,
                    x_max: scroll_rect.width(),
                    y_max: scroll_rect.height(),
                };
        }

        let mut bounds = *matrix * self.self_bounds(mode);

        if let Some(ctr) = self.as_container() {
            for child in ctr.iter_render_list() {
                let matrix = *matrix * child.base().matrix();
                bounds = bounds.union(&child.bounds_with_transform(&matrix, mode));
            }
        }

        bounds
    }

    /// Gets the **render bounds** of this object and all its children.
    /// This differs from the bounds that are exposed to Flash, in two main ways:
    /// - It may be larger if filters are applied which will increase the size of what's shown
    /// - It does not respect scroll rects
    ///
    /// Uses `BoundsMode::Engine` as this is for rendering purposes.
    fn render_bounds_with_transform(
        self,
        matrix: &Matrix,
        include_own_filters: bool,
        view_matrix: &Matrix,
    ) -> Rectangle<Twips> {
        let Some(_bounds_guard) = DisplayObjectRecursionGuard::enter(
            &BOUNDS_RECURSION_DEPTH,
            "render_bounds",
            self.into(),
        ) else {
            return *matrix * self.self_bounds(BoundsMode::Engine);
        };

        let mut bounds = *matrix * self.self_bounds(BoundsMode::Engine);

        if let Some(ctr) = self.as_container() {
            for child in ctr.iter_render_list() {
                let matrix = *matrix * child.base().matrix();
                bounds =
                    bounds.union(&child.render_bounds_with_transform(&matrix, true, view_matrix));
            }
        }

        if include_own_filters && bounds.is_valid() {
            for mut filter in self.filters().iter().cloned() {
                filter.scale(view_matrix.a, view_matrix.d);
                bounds = filter.calculate_dest_rect(bounds);
            }
        }

        bounds
    }

    #[no_dynamic]
    fn place_frame(self) -> u16 {
        self.base().place_frame()
    }

    #[no_dynamic]
    fn set_place_frame(self, frame: u16) {
        self.base().set_place_frame(frame)
    }

    /// Sets the matrix of this object.
    /// This does NOT invalidate the cache, as it's often used with other operations.
    /// It is the callers responsibility to do so.
    fn set_matrix(self, matrix: Matrix) {
        self.base().set_matrix(matrix);
    }

    /// Sets the color transform of this object.
    /// This does NOT invalidate the cache, as it's often used with other operations.
    /// It is the callers responsibility to do so.
    #[no_dynamic]
    fn set_color_transform(self, color_transform: ColorTransform) {
        let changed = self.base().set_color_transform(color_transform);
        if changed && aqw_flicker_probe_enabled() {
            note_colour_write(self, &color_transform);
        }
        if changed && let Some(parent) = self.parent() {
            parent.invalidate_cached_bitmap();
        }
    }

    /// Sets the perspective projection of this object.
    /// This invalidates any ancestors cacheAsBitmap automatically.
    fn set_perspective_projection(self, perspective_projection: Option<PerspectiveProjection>) {
        if self
            .base()
            .set_perspective_projection(perspective_projection)
            && let Some(parent) = self.parent()
        {
            // Self-transform changes are automatically handled,
            // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
            parent.invalidate_cached_bitmap();
        }
    }

    /// Should only be used to implement 'Transform.concatenatedMatrix'
    #[no_dynamic]
    fn local_to_global_matrix_without_own_scroll_rect(self) -> Matrix {
        let mut node = self.parent();
        let mut matrix = self.base().matrix();
        while let Some(display_object) = node {
            // We want to transform to Stage-local coordinates,
            // so do *not* apply the Stage's matrix
            if display_object.as_stage().is_some() {
                break;
            }
            if let Some(rect) = display_object.scroll_rect() {
                matrix = Matrix::translate(-rect.x_min, -rect.y_min) * matrix;
            }
            matrix = display_object.base().matrix() * matrix;
            node = display_object.parent();
        }
        matrix
    }

    /// Returns the matrix for transforming from this object's local space to global stage space.
    fn local_to_global_matrix(self) -> Matrix {
        let mut matrix = Matrix::IDENTITY;
        if let Some(rect) = self.scroll_rect() {
            matrix = Matrix::translate(-rect.x_min, -rect.y_min) * matrix;
        }
        self.local_to_global_matrix_without_own_scroll_rect() * matrix
    }

    /// Returns the matrix for transforming from global stage to this object's local space.
    /// `None` is returned if the object has zero scale.
    #[no_dynamic]
    fn global_to_local_matrix(self) -> Option<Matrix> {
        self.local_to_global_matrix().inverse()
    }

    /// Converts a local position to a global stage position
    #[no_dynamic]
    fn local_to_global(self, local: Point<Twips>) -> Point<Twips> {
        self.local_to_global_matrix() * local
    }

    /// Converts a local position on the stage to a local position on this display object
    /// Returns `None` if the object has zero scale.
    #[no_dynamic]
    fn global_to_local(self, global: Point<Twips>) -> Option<Point<Twips>> {
        self.global_to_local_matrix().map(|matrix| matrix * global)
    }

    /// Converts the mouse position on the stage to a local position on this display object.
    /// If the object has zero scale, then the stage `TWIPS_TO_PIXELS` matrix will be used.
    /// This matches Flash's behavior for `mouseX`/`mouseY` on an object with zero scale.
    #[no_dynamic]
    fn local_mouse_position(self, context: &UpdateContext<'gc>) -> Point<Twips> {
        let stage = context.stage;
        let pixel_ratio = stage.view_matrix().a;
        let virtual_to_device = Matrix::scale(pixel_ratio, pixel_ratio);

        // Get mouse pos in global device pixels
        let global_twips = *context.mouse_position;
        let global_device_twips = virtual_to_device * global_twips;
        let global_device_pixels = Matrix::TWIPS_TO_PIXELS * global_device_twips;

        // Make transformation matrix
        let local_twips_to_global_twips = self.local_to_global_matrix();
        let twips_to_device_pixels = virtual_to_device * Matrix::TWIPS_TO_PIXELS;
        let local_twips_to_global_device_pixels =
            twips_to_device_pixels * local_twips_to_global_twips;
        let global_device_pixels_to_local_twips = local_twips_to_global_device_pixels
            .inverse()
            .unwrap_or(Matrix::IDENTITY);

        // Get local mouse position in twips
        global_device_pixels_to_local_twips * global_device_pixels
    }

    /// The `x` position in pixels of this display object in local space.
    /// Returned by the `_x`/`x` ActionScript properties.
    fn x(self) -> Twips {
        self.base().x()
    }

    /// Sets the `x` position in pixels of this display object in local space.
    /// Set by the `_x`/`x` ActionScript properties.
    /// This invalidates any ancestors cacheAsBitmap automatically.
    fn set_x(self, x: Twips) {
        if self.base().set_x(x)
            && let Some(parent) = self.parent()
        {
            // Self-transform changes are automatically handled,
            // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
            parent.invalidate_cached_bitmap();
        }
    }

    /// The `y` position in pixels of this display object in local space.
    /// Returned by the `_y`/`y` ActionScript properties.
    fn y(self) -> Twips {
        self.base().y()
    }

    /// Sets the `y` position in pixels of this display object in local space.
    /// Set by the `_y`/`y` ActionScript properties.
    /// This invalidates any ancestors cacheAsBitmap automatically.
    fn set_y(self, y: Twips) {
        if self.base().set_y(y)
            && let Some(parent) = self.parent()
        {
            // Self-transform changes are automatically handled,
            // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
            parent.invalidate_cached_bitmap();
        }
    }

    /// The rotation in degrees this display object in local space.
    /// Returned by the `_rotation`/`rotation` ActionScript properties.
    #[no_dynamic]
    fn rotation(self) -> Degrees {
        let degrees = self.base().rotation();
        self.set_scale_rotation_cached();
        degrees
    }

    /// Sets the rotation in degrees this display object in local space.
    /// Set by the `_rotation`/`rotation` ActionScript properties.
    /// This invalidates any ancestors cacheAsBitmap automatically.
    #[no_dynamic]
    fn set_rotation(self, radians: Degrees) {
        if self.base().set_rotation(radians) {
            self.set_scale_rotation_cached();
            if let Some(parent) = self.parent() {
                // Self-transform changes are automatically handled,
                // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
                parent.invalidate_cached_bitmap();
            }
        }
    }

    /// The X axis scale for this display object in local space.
    /// Returned by the `_xscale`/`scaleX` ActionScript properties.
    #[no_dynamic]
    fn scale_x(self) -> Percent {
        let percent = self.base().scale_x();
        self.set_scale_rotation_cached();
        percent
    }

    /// Sets the X axis scale for this display object in local space.
    /// Set by the `_xscale`/`scaleX` ActionScript properties.
    /// This invalidates any ancestors cacheAsBitmap automatically.
    #[no_dynamic]
    fn set_scale_x(self, value: Percent) {
        if self.base().set_scale_x(value) {
            self.set_scale_rotation_cached();
            if let Some(parent) = self.parent() {
                // Self-transform changes are automatically handled,
                // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
                parent.invalidate_cached_bitmap();
            }
        }
    }

    /// The Y axis scale for this display object in local space.
    /// Returned by the `_yscale`/`scaleY` ActionScript properties.
    #[no_dynamic]
    fn scale_y(self) -> Percent {
        let percent = self.base().scale_y();
        self.set_scale_rotation_cached();
        percent
    }

    /// Sets the Y axis scale for this display object in local space.
    /// Returned by the `_yscale`/`scaleY` ActionScript properties.
    /// This invalidates any ancestors cacheAsBitmap automatically.
    #[no_dynamic]
    fn set_scale_y(self, value: Percent) {
        if self.base().set_scale_y(value) {
            self.set_scale_rotation_cached();
            if let Some(parent) = self.parent() {
                // Self-transform changes are automatically handled,
                // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
                parent.invalidate_cached_bitmap();
            }
        }
    }

    /// Gets the pixel width of the AABB containing this display object in local space.
    /// Returned by the ActionScript `_width`/`width` properties.
    fn width(self) -> f64 {
        self.local_bounds(BoundsMode::Script).width().to_pixels()
    }

    /// Sets the pixel width of this display object in local space.
    /// The width is based on the AABB of the object.
    /// Set by the ActionScript `_width`/`width` properties.
    /// This does odd things on rotated clips to match the behavior of Flash.
    fn set_width(self, _context: &mut UpdateContext<'gc>, value: f64) {
        let object_bounds = self.bounds(BoundsMode::Script);
        let object_width = object_bounds.width().to_pixels();
        let object_height = object_bounds.height().to_pixels();
        let aspect_ratio = object_height / object_width;

        let (target_scale_x, target_scale_y) = if object_width != 0.0 {
            (value / object_width, value / object_height)
        } else {
            (0.0, 0.0)
        };

        // No idea about the derivation of this -- figured it out via lots of trial and error.
        // It has to do with the length of the sides A, B of an AABB enclosing the object's OBB with sides a, b:
        // A = sin(t) * a + cos(t) * b
        // B = cos(t) * a + sin(t) * b
        let prev_scale_x = self.scale_x().unit();
        let prev_scale_y = self.scale_y().unit();
        let rotation = self.rotation();
        let cos = f64::abs(f64::cos(rotation.into_radians()));
        let sin = f64::abs(f64::sin(rotation.into_radians()));
        let mut new_scale_x = aspect_ratio * (cos * target_scale_x + sin * target_scale_y)
            / ((cos + aspect_ratio * sin) * (aspect_ratio * cos + sin));
        let mut new_scale_y =
            (sin * prev_scale_x + aspect_ratio * cos * prev_scale_y) / (aspect_ratio * cos + sin);

        if !new_scale_x.is_finite() {
            new_scale_x = 0.0;
        }

        if !new_scale_y.is_finite() {
            new_scale_y = 0.0;
        }

        self.set_scale_x(Percent::from_unit(new_scale_x));
        self.set_scale_y(Percent::from_unit(new_scale_y));
    }

    /// Gets the pixel height of the AABB containing this display object in local space.
    /// Returned by the ActionScript `_height`/`height` properties.
    fn height(self) -> f64 {
        self.local_bounds(BoundsMode::Script).height().to_pixels()
    }

    /// Sets the pixel height of this display object in local space.
    /// Set by the ActionScript `_height`/`height` properties.
    /// This does odd things on rotated clips to match the behavior of Flash.
    fn set_height(self, _context: &mut UpdateContext<'gc>, value: f64) {
        let object_bounds = self.bounds(BoundsMode::Script);
        let object_width = object_bounds.width().to_pixels();
        let object_height = object_bounds.height().to_pixels();
        let aspect_ratio = object_width / object_height;

        let (target_scale_x, target_scale_y) = if object_height != 0.0 {
            (value / object_width, value / object_height)
        } else {
            (0.0, 0.0)
        };

        // No idea about the derivation of this -- figured it out via lots of trial and error.
        // It has to do with the length of the sides A, B of an AABB enclosing the object's OBB with sides a, b:
        // A = sin(t) * a + cos(t) * b
        // B = cos(t) * a + sin(t) * b
        let prev_scale_x = self.scale_x().unit();
        let prev_scale_y = self.scale_y().unit();
        let rotation = self.rotation();
        let cos = f64::abs(f64::cos(rotation.into_radians()));
        let sin = f64::abs(f64::sin(rotation.into_radians()));
        let mut new_scale_x =
            (aspect_ratio * cos * prev_scale_x + sin * prev_scale_y) / (aspect_ratio * cos + sin);
        let mut new_scale_y = aspect_ratio * (sin * target_scale_x + cos * target_scale_y)
            / ((cos + aspect_ratio * sin) * (aspect_ratio * cos + sin));

        if !new_scale_x.is_finite() {
            new_scale_x = 0.0;
        }

        if !new_scale_y.is_finite() {
            new_scale_y = 0.0;
        }

        self.set_scale_x(Percent::from_unit(new_scale_x));
        self.set_scale_y(Percent::from_unit(new_scale_y));
    }

    #[no_dynamic]
    fn ratio(self) -> u16 {
        self.base().ratio.get()
    }

    #[no_dynamic]
    fn set_ratio(self, context: &mut UpdateContext<'gc>, ratio: u16) {
        self.base().ratio.set(ratio);
        self.invalidate_cached_bitmap();
        self.on_ratio_changed(context, ratio);
    }

    fn on_ratio_changed(self, _context: &mut UpdateContext<'gc>, _new_ratio: u16) {}

    /// The opacity of this display object.
    /// 1 is fully opaque.
    /// Returned by the `_alpha`/`alpha` ActionScript properties.
    #[no_dynamic]
    fn alpha(self) -> f64 {
        self.base().alpha()
    }

    /// Sets the opacity of this display object.
    /// 1 is fully opaque.
    /// Set by the `_alpha`/`alpha` ActionScript properties.
    /// This invalidates any cacheAsBitmap automatically.
    #[no_dynamic]
    fn set_alpha(self, value: f64) {
        if self.base().set_alpha(value)
            && let Some(parent) = self.parent()
        {
            // Self-transform changes are automatically handled
            parent.invalidate_cached_bitmap();
        }
    }

    #[no_dynamic]
    fn name(self) -> Option<AvmString<'gc>> {
        self.base().name()
    }

    #[no_dynamic]
    fn set_name(self, mc: &Mutation<'gc>, name: AvmString<'gc>) {
        DisplayObjectBase::set_name(Gc::write(mc, self.base()), name)
    }

    fn filters(self) -> Ref<'gc, [Filter]> {
        Gc::as_ref(self.base()).filters()
    }

    fn set_filters(self, filters: Box<[Filter]>) {
        if self.base().set_filters(filters) {
            self.invalidate_cached_bitmap();
        }
    }

    /// Returns the dot-syntax path to this display object, e.g. `_level0.foo.clip`
    #[no_dynamic]
    fn path(self) -> WString {
        if let Some(parent) = self.avm1_parent() {
            let mut path = parent.path();
            path.push_byte(b'.');
            if let Some(name) = self.name() {
                path.push_str(&name);
            }
            path
        } else {
            WString::from_utf8_owned(format!("_level{}", self.depth()))
        }
    }

    /// Returns the Flash 4 slash-syntax path to this display object, e.g. `/foo/clip`.
    /// Returned by the `_target` property in AVM1.
    #[no_dynamic]
    fn slash_path(self) -> WString {
        fn build_slash_path(object: DisplayObject<'_>) -> WString {
            if let Some(parent) = object.avm1_parent() {
                let mut path = build_slash_path(parent);
                path.push_byte(b'/');
                if let Some(name) = object.name() {
                    path.push_str(&name);
                }
                path
            } else {
                let level = object.depth();
                if level == 0 {
                    // _level0 does not append its name in slash syntax.
                    WString::new()
                } else {
                    // Other levels do append their name.
                    WString::from_utf8_owned(format!("_level{level}"))
                }
            }
        }

        if self.avm1_parent().is_some() {
            build_slash_path(self)
        } else {
            // _target of _level0 should just be '/'.
            WString::from_unit(b'/'.into())
        }
    }

    #[no_dynamic]
    fn clip_depth(self) -> Depth {
        self.base().clip_depth()
    }

    #[no_dynamic]
    fn set_clip_depth(self, depth: Depth) {
        self.base().set_clip_depth(depth);
    }

    /// Retrieve the parent of this display object.
    ///
    /// This version of the function merely exposes the display object parent,
    /// without any further filtering.
    #[no_dynamic]
    fn parent(self) -> Option<DisplayObject<'gc>> {
        self.base().parent()
    }

    #[no_dynamic]
    fn is_in_detached_aqw_avatar_loader(self) -> bool {
        let mut current: Option<DisplayObject<'gc>> = Some(self);

        while let Some(display_object) = current {
            if let DisplayObject::LoaderDisplay(loader) = display_object
                && loader.is_detached_aqw_avatar_loader()
            {
                return true;
            }

            current = display_object.parent();
        }

        false
    }

    #[no_dynamic]
    fn is_in_currently_detached_aqw_avatar_loader(self) -> bool {
        let mut current: Option<DisplayObject<'gc>> = Some(self);

        while let Some(display_object) = current {
            if let DisplayObject::LoaderDisplay(loader) = display_object
                && loader.is_currently_parentless_aqw_avatar_loader()
            {
                return true;
            }

            current = display_object.parent();
        }

        false
    }

    /// Set the parent of this display object.
    #[no_dynamic]
    fn set_parent(self, context: &mut UpdateContext<'gc>, parent: Option<DisplayObject<'gc>>) {
        let had_parent = self.parent().is_some();
        let write = Gc::write(context.gc(), self.base());
        DisplayObjectBase::set_parent_ignoring_orphan_list(write, parent);
        let parent_removed = had_parent && parent.is_none();
        let parent_added = !had_parent && parent.is_some();

        self.mark_subtree_needs_frame(context);
        if let Some(parent) = parent {
            parent.mark_subtree_needs_frame(context);
        }

        if parent_removed {
            if let Some(int) = self.as_interactive() {
                int.drop_focus(context);
            }

            self.on_parent_removed(context);
        } else if parent_added {
            self.on_parent_added(context);
        }
    }

    fn on_parent_added(self, _context: &mut UpdateContext<'gc>) {}

    /// This method is called when the parent is removed.
    /// It may be overwritten to inject some implementation-specific behavior.
    fn on_parent_removed(self, _context: &mut UpdateContext<'gc>) {}

    /// Retrieve the parent of this display object.
    ///
    /// This version of the function implements the concept of parenthood as
    /// seen in AVM1. Notably, it disallows access to the `Stage` and to
    /// non-AVM1 DisplayObjects; for an unfiltered concept of parent,
    /// use the `parent` method.
    #[no_dynamic]
    fn avm1_parent(self) -> Option<DisplayObject<'gc>> {
        self.parent()
            .filter(|p| p.as_stage().is_none())
            .filter(|p| !p.movie().is_action_script_3())
    }

    /// Retrieve the parent of this display object.
    ///
    /// This version of the function implements the concept of parenthood as
    /// seen in AVM2. Notably, it disallows access to non-container parents.
    #[no_dynamic]
    fn avm2_parent(self) -> Option<DisplayObject<'gc>> {
        self.parent().filter(|p| p.as_container().is_some())
    }

    #[no_dynamic]
    fn masker(self) -> Option<DisplayObject<'gc>> {
        self.base().masker()
    }

    #[no_dynamic]
    fn set_masker(
        self,
        mc: &Mutation<'gc>,
        node: Option<DisplayObject<'gc>>,
        remove_old_link: bool,
    ) {
        if remove_old_link {
            let old_masker = self.base().masker();
            if let Some(old_masker) = old_masker {
                old_masker.set_maskee(mc, None, false);
            }
            if let Some(parent) = self.parent() {
                // Masks are natively handled by cacheAsBitmap - don't invalidate self, only parents
                parent.invalidate_cached_bitmap();
            }
        }
        DisplayObjectBase::set_masker(Gc::write(mc, self.base()), node);
    }

    #[no_dynamic]
    fn maskee(self) -> Option<DisplayObject<'gc>> {
        self.base().maskee()
    }

    #[no_dynamic]
    fn set_maskee(
        self,
        mc: &Mutation<'gc>,
        node: Option<DisplayObject<'gc>>,
        remove_old_link: bool,
    ) {
        if remove_old_link {
            let old_maskee = self.base().maskee();
            if let Some(old_maskee) = old_maskee {
                old_maskee.set_masker(mc, None, false);
            }
            self.invalidate_cached_bitmap();
        }
        DisplayObjectBase::set_maskee(Gc::write(mc, self.base()), node);
    }

    #[no_dynamic]
    fn get_render_mask(self) -> RenderMask<'gc> {
        match self.masker() {
            None => RenderMask::None,
            Some(mask) if self.is_bitmap_cached() && mask.is_bitmap_cached() => {
                RenderMask::Alpha(mask)
            }
            Some(mask) => RenderMask::Stencil(mask),
        }
    }

    /// High level method for setting the mask. Sets both masker and maskee.
    ///
    /// Equivalent to setting the mask from AVM.
    #[no_dynamic]
    fn set_mask(self, mask: Option<DisplayObject<'gc>>, mc: &Mutation<'gc>) {
        self.set_clip_depth(0);
        self.set_masker(mc, mask, true);
        if let Some(mask) = mask {
            mask.set_clip_depth(0);
            mask.set_maskee(mc, Some(self), true);
        }
    }

    #[no_dynamic]
    fn scroll_rect(self) -> Option<Rectangle<Twips>> {
        self.base().scroll_rect.get()
    }

    #[no_dynamic]
    fn next_scroll_rect(self) -> Rectangle<Twips> {
        self.base().next_scroll_rect.get()
    }

    #[no_dynamic]
    fn set_next_scroll_rect(self, rectangle: Rectangle<Twips>) {
        self.base().next_scroll_rect.set(rectangle);

        // Scroll rect is natively handled by cacheAsBitmap - don't invalidate self, only parents
        if let Some(parent) = self.parent() {
            parent.invalidate_cached_bitmap();
        }
    }

    #[no_dynamic]
    fn scaling_grid(self) -> Rectangle<Twips> {
        self.base().scaling_grid.get()
    }

    #[no_dynamic]
    fn set_scaling_grid(self, rect: Rectangle<Twips>) {
        self.base().scaling_grid.set(rect);
    }

    #[no_dynamic]
    /// Whether this object has been removed. Only applies to AVM1.
    fn avm1_removed(self) -> bool {
        self.base().avm1_removed()
    }

    #[no_dynamic]
    // Sets whether this object has been removed. Only applies to AVM1
    fn set_avm1_removed(self, value: bool) {
        self.base().set_avm1_removed(value)
    }

    #[no_dynamic]
    /// Is this object waiting to be removed on the start of the next frame
    fn avm1_pending_removal(self) -> bool {
        self.base().avm1_pending_removal()
    }

    #[no_dynamic]
    fn set_avm1_pending_removal(self, value: bool) {
        self.base().set_avm1_pending_removal(value)
    }

    /// Whether this display object is visible.
    /// Invisible objects are not rendered, but otherwise continue to exist normally.
    /// Returned by the `_visible`/`visible` ActionScript properties.
    #[no_dynamic]
    fn visible(self) -> bool {
        self.base().visible()
    }

    /// Sets whether this display object will be visible.
    /// Invisible objects are not rendered, but otherwise continue to exist normally.
    /// Returned by the `_visible`/`visible` ActionScript properties.
    #[no_dynamic]
    fn set_visible(self, context: &mut UpdateContext<'gc>, value: bool) {
        if self.base().set_visible(value)
            && let Some(parent) = self.parent()
        {
            // We don't need to invalidate ourselves, we're just toggling if the bitmap is rendered.
            parent.invalidate_cached_bitmap();
        }

        if !value && let Some(int) = self.as_interactive() {
            // The focus is dropped when it's made invisible.
            int.drop_focus(context);
        }
    }

    #[no_dynamic]
    fn meta_data(self) -> Option<Avm2Object<'gc>> {
        self.base().meta_data()
    }

    #[no_dynamic]
    fn set_meta_data(self, mc: &Mutation<'gc>, value: Avm2Object<'gc>) {
        DisplayObjectBase::set_meta_data(Gc::write(mc, self.base()), value);
    }

    /// The blend mode used when rendering this display object.
    /// Values other than the default `BlendMode::Normal` implicitly cause cache-as-bitmap behavior.
    #[no_dynamic]
    fn blend_mode(self) -> ExtendedBlendMode {
        self.base().blend_mode()
    }

    /// Sets the blend mode used when rendering this display object.
    /// Values other than the default `BlendMode::Normal` implicitly cause cache-as-bitmap behavior.
    #[no_dynamic]
    fn set_blend_mode(self, value: ExtendedBlendMode) {
        if self.base().set_blend_mode(value)
            && let Some(parent) = self.parent()
        {
            // We don't need to invalidate ourselves, we're just toggling how the bitmap is rendered.

            // Note that Flash does not always invalidate on changing the blend mode;
            // but that's a bug we don't need to copy :)
            parent.invalidate_cached_bitmap();
        }
    }

    #[no_dynamic]
    fn blend_shader(self) -> Option<PixelBenderShaderHandle> {
        self.base().blend_shader()
    }

    #[no_dynamic]
    fn set_blend_shader(self, value: Option<PixelBenderShaderHandle>) {
        self.base().set_blend_shader(value);
        self.set_blend_mode(ExtendedBlendMode::Shader);
    }

    #[no_dynamic]
    /// The opaque background color of this display object.
    fn opaque_background(self) -> Option<Color> {
        self.base().opaque_background()
    }

    /// Sets the opaque background color of this display object.
    /// The bounding box of the display object will be filled with the given color. This also
    /// triggers cache-as-bitmap behavior. Only solid backgrounds are supported; the alpha channel
    /// is ignored.
    #[no_dynamic]
    fn set_opaque_background(self, value: Option<Color>) {
        if self.base().set_opaque_background(value) {
            self.invalidate_cached_bitmap();
        }
    }

    /// Whether this display object represents the root of loaded content.
    #[no_dynamic]
    fn is_root(self) -> bool {
        self.base().is_root()
    }

    /// Sets whether this display object represents the root of loaded content.
    #[no_dynamic]
    fn set_is_root(self, value: bool) {
        self.base().set_is_root(value);
    }

    /// The sound transform for sounds played inside this display object.
    #[no_dynamic]
    fn set_sound_transform(
        self,
        context: &mut UpdateContext<'gc>,
        sound_transform: SoundTransform,
    ) {
        self.base().set_sound_transform(sound_transform);
        context.set_sound_transforms_dirty();
    }

    /// Whether this display object is used as the _root of itself and its children.
    /// Returned by the `_lockroot` ActionScript property.
    #[no_dynamic]
    fn lock_root(self) -> bool {
        self.base().lock_root()
    }

    /// Sets whether this display object is used as the _root of itself and its children.
    /// Returned by the `_lockroot` ActionScript property.
    #[no_dynamic]
    fn set_lock_root(self, value: bool) {
        self.base().set_lock_root(value);
    }

    /// Whether this display object has been transformed by ActionScript.
    /// When this flag is set, changes from SWF `PlaceObject` tags are ignored.
    #[no_dynamic]
    fn transformed_by_script(self) -> bool {
        self.base().transformed_by_script()
    }

    /// Sets whether this display object has been transformed by ActionScript.
    /// When this flag is set, changes from SWF `PlaceObject` tags are ignored.
    #[no_dynamic]
    fn set_transformed_by_script(self, value: bool) {
        self.base().set_transformed_by_script(value)
    }

    /// Whether this display object prefers to be cached into a bitmap rendering.
    /// This is the PlaceObject `cacheAsBitmap` flag - and may be overridden if filters are applied.
    /// Consider `is_bitmap_cached` for if a bitmap cache is actually in use.
    #[no_dynamic]
    fn is_bitmap_cached_preference(self) -> bool {
        self.base().is_bitmap_cached_preference()
    }

    /// Whether this display object is using a bitmap cache, whether by preference or necessity.
    #[no_dynamic]
    fn is_bitmap_cached(self) -> bool {
        self.base().cell.borrow().cache.is_some()
    }

    #[no_dynamic]
    fn clear_bitmap_cache(self) {
        if let Some(cache) = &mut *self.base().bitmap_cache_mut() {
            cache.clear();
        }
    }

    /// Explicitly sets the preference of this display object to be cached into a bitmap rendering.
    /// Note that the object will still be bitmap cached if a filter is active.
    #[no_dynamic]
    fn set_bitmap_cached_preference(self, value: bool) {
        self.base().set_bitmap_cached_preference(value)
    }

    /// Whether this display object has a scroll rectangle applied.
    #[no_dynamic]
    fn has_scroll_rect(self) -> bool {
        self.base().has_scroll_rect()
    }

    /// Sets whether this display object has a scroll rectangle applied.
    #[no_dynamic]
    fn set_has_scroll_rect(self, value: bool) {
        self.base().set_has_scroll_rect(value)
    }

    /// Whether this display object has been created by ActionScript 1/2.
    #[no_dynamic]
    fn placed_by_avm1_script(self) -> bool {
        self.base().placed_by_avm1_script()
    }

    /// Sets whether this display object has been created by ActionScript 1/2.
    #[no_dynamic]
    fn set_placed_by_avm1_script(self, value: bool) {
        self.base().set_placed_by_avm1_script(value);
    }

    /// Whether this display object has been created by ActionScript 3.
    /// When this flag is set, changes from SWF `RemoveObject` tags are
    /// ignored.
    #[no_dynamic]
    fn placed_by_avm2_script(self) -> bool {
        self.base().placed_by_avm2_script()
    }

    /// When this flag is set, changes from SWF `RemoveObject` tags are
    /// ignored.
    #[no_dynamic]
    fn set_placed_by_avm2_script(self, value: bool) {
        self.base().set_placed_by_avm2_script(value)
    }

    #[no_dynamic]
    fn manual_frame_construct(&self) -> bool {
        self.base().manual_frame_construct()
    }

    /// When this flag is set, the object will not be instantiated in-line with
    /// normal frame construction by `MovieClip::construct_frame`.
    #[no_dynamic]
    fn set_manual_frame_construct(&self, value: bool) {
        self.base().set_manual_frame_construct(value);
    }

    /// Whether this display object has been instantiated by the timeline.
    /// When this flag is set, attempts to change the object's name from AVM2
    /// throw an exception.
    #[no_dynamic]
    fn instantiated_by_timeline(self) -> bool {
        self.base().instantiated_by_timeline()
    }

    /// Sets whether this display object has been instantiated by the timeline.
    /// When this flag is set, attempts to change the object's name from AVM2
    /// throw an exception.
    #[no_dynamic]
    fn set_instantiated_by_timeline(self, value: bool) {
        self.base().set_instantiated_by_timeline(value);
    }

    /// Whether this display object was placed by a SWF tag with an explicit
    /// name.
    ///
    /// When this flag is set, the object will attempt to set a dynamic property
    /// on the parent with the same name as itself.
    #[no_dynamic]
    fn has_explicit_name(self) -> bool {
        self.base().has_explicit_name()
    }

    /// Sets whether this display object was placed by a SWF tag with an
    /// explicit name.
    ///
    /// When this flag is set, the object will attempt to set a dynamic property
    /// on the parent with the same name as itself.
    #[no_dynamic]
    fn set_has_explicit_name(self, value: bool) {
        self.base().set_has_explicit_name(value);
    }
    fn state(&self) -> Option<ButtonState> {
        None
    }

    fn set_state(self, _context: &mut UpdateContext<'gc>, _state: ButtonState) {}

    /// Run any start-of-frame actions for this display object.
    ///
    /// When fired on `Stage`, this also emits the AVM2 `enterFrame` broadcast.
    fn enter_frame(self, _context: &mut UpdateContext<'gc>) {}

    /// Construct all display objects that the timeline indicates should exist
    /// this frame, and their children.
    ///
    /// This function should ensure the following, from the point of view of
    /// downstream VMs:
    ///
    /// 1. That the object itself has been allocated, if not constructed
    /// 2. That newly created children have been instantiated and are present
    ///    as properties on the class
    fn construct_frame(self, _context: &mut UpdateContext<'gc>) {}

    #[no_dynamic]
    fn can_skip_frame_pass(self, context: &UpdateContext<'gc>) -> bool {
        *context.aqw_nested_goto && !self.base().subtree_needs_frame() && !frame_skip_disabled()
    }

    #[no_dynamic]
    fn aqw_subtree_clean(self) -> bool {
        !self.base().subtree_needs_frame()
    }

    fn needs_frame_construction(self) -> bool {
        false
    }

    #[no_dynamic]
    fn settle_subtree_needs_frame(self, children_need: bool) {
        self.base()
            .set_subtree_needs_frame(children_need || self.needs_frame_construction());
    }

    #[no_dynamic]
    fn mark_subtree_needs_frame_deep(self, context: &mut UpdateContext<'gc>) {
        self.mark_subtree_needs_frame(context);
        if let Some(container) = self.as_container() {
            for child in container.iter_render_list() {
                child.mark_subtree_needs_frame_deep(context);
            }
        }
    }

    #[no_dynamic]
    fn mark_subtree_needs_frame(self, context: &mut UpdateContext<'gc>) {
        self.base().set_subtree_needs_frame(true);

        let mut top: DisplayObject<'gc> = self;
        let mut node = self.parent();
        while let Some(current) = node {
            top = current;
            if current.base().subtree_needs_frame() {
                return;
            }
            current.base().set_subtree_needs_frame(true);
            node = current.parent();
        }

        if !matches!(top, DisplayObject::Stage(_)) {
            context.orphan_manager.note_orphan_root_dirty(top);
        }
    }

    /// To be called when an AVM2 display object has finished being constructed.
    ///
    /// This function must be called once and ONLY once, after the object's
    /// AVM2 side has been constructed. Typically, this is in construct_frame,
    /// unless your object needs to construct itself earlier or later. When
    /// this function is called on the child, it will fire its add events and,
    /// if possible, set a named property on the parent matching the name of
    /// the object.
    ///
    /// This still needs to be called for objects placed by AVM2, since we
    /// need to stop the underlying MovieClip if the constructed class
    /// does not extend MovieClip.
    ///
    /// Since we construct AVM2 display objects after they are allocated and
    /// placed on the render list, these steps have to be done by the child
    /// object to signal to its parent that it was added.
    #[no_dynamic]
    #[inline(never)]
    fn on_construction_complete(self, context: &mut UpdateContext<'gc>) {
        let placed_by_avm2_script = self.placed_by_avm2_script();
        self.fire_added_events(context);
        // Check `self.placed_by_avm2_script()` before we fire events, since those
        // events might `placed_by_avm2_script`
        if !placed_by_avm2_script {
            self.set_on_parent_field(context);
        }

        if let Some(movie) = self.as_movie_clip() {
            let obj = movie
                .object2()
                .expect("MovieClip object should have been constructed");
            let movieclip_class = context.avm2.classes().movieclip.inner_class_definition();
            // It's possible to have a DefineSprite tag with multiple frames, but have
            // the corresponding `SymbolClass` *not* extend `MovieClip` (e.g. extending `Sprite` directly.)
            // When this occurs, Flash Player will run the first frame, and immediately stop.
            // However, Flash Player runs frames for the root movie clip, even if it doesn't extend `MovieClip`.
            if !obj.is_of_type(movieclip_class) && !movie.is_root() {
                movie.stop(context);
            }
            movie.set_initialized();
        }
    }

    #[no_dynamic]
    fn fire_added_events(self, context: &mut UpdateContext<'gc>) {
        if !self.placed_by_avm2_script() {
            // Since we construct AVM2 display objects after they are
            // allocated and placed on the render list, we have to emit all
            // events after this point.
            //
            // Children added to buttons by the timeline do not emit events.
            if self.parent().and_then(|p| p.as_avm2_button()).is_none() {
                dispatch_added_event_only(self, context);
                if self.avm2_stage(context).is_some() {
                    dispatch_added_to_stage_event_only(self, context);
                }
            }
        }
    }

    #[no_dynamic]
    fn set_on_parent_field(self, context: &mut UpdateContext<'gc>) {
        if self.has_explicit_name()
            && let Some(parent) = self.parent().and_then(|p| p.object2())
        {
            let parent = Avm2Value::from(parent);

            if let Some(child) = self.object2()
                && let Some(name) = self.name()
            {
                let domain = context
                    .library
                    .library_for_movie(self.movie())
                    .unwrap()
                    .avm2_domain();

                let mut activation = Avm2Activation::from_domain(context, domain);
                let multiname = Avm2Multiname::new(activation.avm2().find_public_namespace(), name);
                let set_result = parent.init_property(&multiname, child.into(), &mut activation);

                if let Err(err) = set_result {
                    Avm2::uncaught_error(
                        &mut activation,
                        Some(self),
                        err,
                        &format!("Error setting AVM2 child named \"{}\"", name),
                    );
                }
            }
        }
    }

    /// Run any frame scripts (if they exist and this object needs to run them).
    fn run_frame_scripts(self, context: &mut UpdateContext<'gc>) {
        if self.can_skip_frame_pass(context) {
            return;
        }

        let Some(_frame_script_guard) = DisplayObjectRecursionGuard::enter(
            &FRAME_SCRIPT_RECURSION_DEPTH,
            "frame_scripts",
            self.into(),
        ) else {
            return;
        };

        let mut children_need = false;
        if let Some(container) = self.as_container() {
            for child in container.iter_render_list() {
                if child.can_skip_frame_pass(context) {
                    continue;
                }
                child.run_frame_scripts(context);
                children_need |= child.base().subtree_needs_frame();
            }
        }
        self.settle_subtree_needs_frame(children_need);
    }

    /// Called before the child is about to be rendered.
    /// Note that this happens even if the child is invisible
    /// (as long as the child is still on a render list)
    #[no_dynamic]
    fn pre_render(self, _context: &mut RenderContext<'_, 'gc>) {
        let this = self.base();
        if aqw_diagnostics_enabled()
            && this.cache_invalidated()
            && is_aqw_movie_url(self.movie().url())
        {
            note_content_dirty_frame(self.into());
        }
        this.clear_invalidate_flag();
        this.scroll_rect
            .set(this.has_scroll_rect().then(|| this.next_scroll_rect.get()));
    }

    fn render_self(self, _context: &mut RenderContext<'_, 'gc>) {}

    #[no_dynamic]
    fn render(self, context: &mut RenderContext<'_, 'gc>) {
        self.render_with_options(context, Default::default())
    }

    fn render_with_options(self, context: &mut RenderContext<'_, 'gc>, options: RenderOptions) {
        render_base(self.into(), context, options)
    }

    #[cfg(not(feature = "avm_debug"))]
    #[no_dynamic]
    fn display_render_tree(self, _depth: usize) {}

    #[cfg(feature = "avm_debug")]
    #[no_dynamic]
    fn display_render_tree(self, depth: usize) {
        let mut self_str = &*format!("{self:?}");
        if let Some(end_char) = self_str.find(|c: char| !c.is_ascii_alphanumeric()) {
            self_str = &self_str[..end_char];
        }

        let bounds = self.world_bounds(BoundsMode::Engine);

        let mut classname = "".to_string();
        if let Some(o) = self.object2() {
            classname = format!("{:?}", o.base().class_name());
        }

        println!(
            "{} rel({},{}) abs({},{}) {} {} {} id={} depth={}",
            " ".repeat(depth),
            self.x(),
            self.y(),
            bounds.x_min.to_pixels(),
            bounds.y_min.to_pixels(),
            classname,
            self.name().map(|s| s.to_string()).unwrap_or_default(),
            self_str,
            self.id(),
            depth
        );

        if let Some(ctr) = self.as_container() {
            ctr.recurse_render_tree(depth + 1);
        }
    }

    fn avm1_unload(self, context: &mut UpdateContext<'gc>) {
        // Unload children.
        if let Some(ctr) = self.as_container() {
            for child in ctr.iter_render_list() {
                child.avm1_unload(context);
            }
        }

        if let Some(node) = self.maskee() {
            node.set_masker(context.gc(), None, true);
        } else if let Some(node) = self.masker() {
            node.set_maskee(context.gc(), None, true);
        }

        // Unregister any text field variable bindings, and replace them on the unbound list.
        Avm1TextFieldBinding::unregister_bindings(self.into(), context);

        self.set_avm1_removed(true);
    }

    fn avm1_text_field_bindings(&self) -> Option<Ref<'_, [Avm1TextFieldBinding<'gc>]>> {
        None
    }

    fn avm1_text_field_bindings_mut(
        &self,
        _mc: &Mutation<'gc>,
    ) -> Option<RefMut<'_, Vec<Avm1TextFieldBinding<'gc>>>> {
        None
    }

    #[no_dynamic]
    fn apply_place_object(self, context: &mut UpdateContext<'gc>, place_object: &swf::PlaceObject) {
        // PlaceObject tags only apply if this object has not been dynamically moved by AS code.
        if !self.transformed_by_script() {
            if let Some(matrix) = place_object.matrix {
                self.set_matrix(matrix.into());
                if let Some(parent) = self.parent() {
                    // Self-transform changes are automatically handled,
                    // we only want to inform ancestors to avoid unnecessary invalidations for tx/ty
                    parent.invalidate_cached_bitmap();
                }
            }
            if let Some(color_transform) = &place_object.color_transform {
                self.set_color_transform(*color_transform);
                if let Some(parent) = self.parent() {
                    parent.invalidate_cached_bitmap();
                }
            }
            if let Some(ratio) = place_object.ratio {
                self.set_ratio(context, ratio);
            }
            if let Some(is_bitmap_cached) = place_object.is_bitmap_cached {
                self.set_bitmap_cached_preference(is_bitmap_cached);
            }
            if let Some(blend_mode) = place_object.blend_mode {
                self.set_blend_mode(blend_mode.into());
            }
            if self.swf_version() >= 11 {
                if let Some(visible) = place_object.is_visible {
                    self.set_visible(context, visible);
                }
                if let Some(mut color) = place_object.background_color {
                    let color = if color.a > 0 {
                        // Force opaque background to have no transpranecy.
                        color.a = 255;
                        Some(color)
                    } else {
                        None
                    };
                    self.set_opaque_background(color);
                }
            }
            if let Some(filters) = &place_object.filters {
                self.set_filters(filters.iter().map(Filter::from).collect());
            }
            // Purposely omitted properties:
            // name, clip_depth, clip_actions
            // These properties are only set on initial placement in `MovieClip::instantiate_child`
            // and can not be modified by subsequent PlaceObject tags.
        }
    }

    /// Called when this object should be replaced by a PlaceObject tag.
    fn replace_with(self, _context: &mut UpdateContext<'gc>, _id: CharacterId) {
        // Noop for most symbols; only shapes can replace their innards with another Graphic.
    }

    fn object1(self) -> Option<Avm1Object<'gc>>;

    #[no_dynamic]
    fn object1_or_undef(self) -> Avm1Value<'gc> {
        self.object1()
            .map(|o| o.into())
            .unwrap_or(Avm1Value::Undefined)
    }

    #[no_dynamic]
    fn object1_or_null(self) -> Avm1Value<'gc> {
        self.object1().map(|o| o.into()).unwrap_or(Avm1Value::Null)
    }

    /// Equivalent to `self.object1_or_undef().coerce_to_object_or_bare()`, but avoids
    /// the need for an activation.
    ///
    /// [MOULINS]: Like `coerce_to_object_bare`, I suspect that usages of this method
    /// are incorrect,
    #[no_dynamic]
    fn object1_or_bare(self, mc: &Mutation<'gc>) -> Avm1Object<'gc> {
        self.object1()
            .unwrap_or_else(|| Avm1Object::new_without_proto(mc))
    }

    fn object2(self) -> Option<Avm2StageObject<'gc>>;

    fn set_object2(self, _context: &mut UpdateContext<'gc>, _to: Avm2StageObject<'gc>) {}

    #[no_dynamic]
    fn object2_or_null(self) -> Avm2Value<'gc> {
        self.object2().map(|o| o.into()).unwrap_or(Avm2Value::Null)
    }

    /// Tests if a given stage position point intersects with the world bounds of this object.
    #[no_dynamic]
    fn hit_test_bounds(self, point: Point<Twips>) -> bool {
        self.world_bounds(BoundsMode::Engine).contains(point)
    }

    /// Tests if a given object's world bounds intersects with the world bounds
    /// of this object.
    #[no_dynamic]
    fn hit_test_object(self, other: DisplayObject<'gc>) -> bool {
        // This is only used in ActionScript so it gets a BoundsMode::Script.
        self.world_bounds(BoundsMode::Script)
            .intersects(&other.world_bounds(BoundsMode::Script))
    }

    /// Tests if a given stage position point intersects within this object, considering the art.
    fn hit_test_shape(
        self,
        _context: &mut UpdateContext<'gc>,
        point: Point<Twips>,
        options: HitTestOptions,
    ) -> bool {
        // Default to using bounding box.
        (!options.contains(HitTestOptions::SKIP_INVISIBLE) || self.visible())
            && self.hit_test_bounds(point)
    }

    fn post_instantiation(
        self,
        _context: &mut UpdateContext<'gc>,
        _init_object: Option<Avm1Object<'gc>>,
        _instantiated_by: Instantiator,
        _run_frame: bool,
    ) {
        // Noop.
    }

    /// Return the version of the SWF that created this movie clip.
    fn swf_version(self) -> u8 {
        self.movie().version()
    }

    /// Return the SWF that defines this display object.
    fn movie(self) -> Arc<SwfMovie>;

    fn loader_info(self) -> Option<LoaderInfoObject<'gc>> {
        None
    }

    fn instantiate(self, gc_context: &Mutation<'gc>) -> DisplayObject<'gc>;

    /// Whether this object can be used as a mask.
    /// If this returns false and this object is used as a mask, the mask will not be applied.
    /// This is used by movie clips to disable the mask when there are no children, for example.
    fn allow_as_mask(self) -> bool {
        true
    }

    /// Obtain the top-most non-Stage parent of the display tree hierarchy.
    ///
    /// This function implements the AVM1 concept of root clips. For the AVM2
    /// version, see `avm2_root`.
    #[no_dynamic]
    fn avm1_root(self) -> DisplayObject<'gc> {
        let mut root = self;
        loop {
            if root.lock_root() {
                break;
            }
            if let Some(parent) = root.avm1_parent() {
                if !parent.movie().is_action_script_3() {
                    root = parent;
                } else {
                    // We've traversed upwards into a loader AVM2 movie, so break.
                    break;
                }
            } else {
                break;
            }
        }
        root
    }

    /// `avm1_root`, but disregards _lockroot
    #[no_dynamic]
    fn avm1_root_no_lock(self) -> DisplayObject<'gc> {
        let mut root = self;
        while let Some(parent) = root.avm1_parent() {
            if !parent.movie().is_action_script_3() {
                root = parent;
            } else {
                // We've traversed upwards into a loader AVM2 movie, so break.
                break;
            }
        }
        root
    }

    /// Obtain the top-most Stage or LoaderDisplay object of the display tree hierarchy, for use in mixed AVM.
    #[no_dynamic]
    fn avm1_stage(self) -> DisplayObject<'gc> {
        let mut root = self;
        loop {
            if let Some(parent) = root.parent() {
                if matches!(
                    parent,
                    DisplayObject::LoaderDisplay(_) | DisplayObject::Stage(_)
                ) {
                    return parent;
                }
                root = parent;
            } else {
                return root;
            }
        }
    }

    /// Obtain the top-most non-Stage parent of the display tree hierarchy, if
    /// a suitable object exists.
    ///
    /// This function implements the AVM2 concept of root clips. For the AVM1
    /// version, see `avm1_root`.
    #[no_dynamic]
    fn avm2_root(self) -> Option<DisplayObject<'gc>> {
        let mut parent = Some(self);
        while let Some(p) = parent {
            if p.is_root() {
                return parent;
            }
            if let Some(p_parent) = p.parent()
                && !p_parent.movie().is_action_script_3()
            {
                // We've traversed upwards into a loader AVM1 movie, so return the current parent.
                return parent;
            }
            parent = p.parent();
        }
        None
    }

    /// Obtain the root of the display tree hierarchy, if a suitable object
    /// exists.
    ///
    /// This implements the AVM2 concept of `stage`. Notably, it deliberately
    /// will fail to locate the current player's stage for objects that are not
    /// rooted to the DisplayObject hierarchy correctly. If you just want to
    /// access the current player's stage, grab it from the context.
    #[no_dynamic]
    fn avm2_stage(self, _context: &UpdateContext<'gc>) -> Option<DisplayObject<'gc>> {
        let mut parent = Some(self);
        while let Some(p) = parent {
            if p.as_stage().is_some() {
                return parent;
            }
            parent = p.parent();
        }
        None
    }

    /// Determine if this display object is currently on the stage.
    #[no_dynamic]
    fn is_on_stage(self, context: &UpdateContext<'gc>) -> bool {
        let mut ancestor = self.avm2_parent();
        while let Some(parent) = ancestor {
            if parent.avm2_parent().is_some() {
                ancestor = parent.avm2_parent();
            } else {
                break;
            }
        }

        let ancestor = ancestor.unwrap_or(self);
        DisplayObject::ptr_eq(ancestor, context.stage.into())
    }

    /// Assigns a default instance name `instanceN` to this object.
    #[no_dynamic]
    fn set_default_instance_name(self, context: &mut UpdateContext<'gc>) {
        if self.base().name().is_none() {
            let name = format!("instance{}", *context.instance_counter);
            self.set_name(context.gc(), AvmString::new_utf8(context.gc(), name));
            *context.instance_counter = context.instance_counter.wrapping_add(1);
        }
    }

    /// Assigns a default root name to this object.
    ///
    /// The default root names change based on the AVM configuration of the
    /// clip; AVM2 clips get `rootN` while AVM1 clips get blank strings.
    #[no_dynamic]
    fn set_default_root_name(self, context: &mut UpdateContext<'gc>) {
        if self.movie().is_action_script_3() {
            let name = AvmString::new_utf8(context.gc(), format!("root{}", self.depth() + 1));
            self.set_name(context.gc(), name);
        } else {
            self.set_name(context.gc(), istr!(context, ""));
        }
    }

    /// Inform this object and its ancestors that it has visually changed and must be redrawn.
    /// If this object or any ancestor is marked as cacheAsBitmap, it will invalidate that cache.
    #[no_dynamic]
    fn invalidate_cached_bitmap(self) {
        if self.base().invalidate_cached_bitmap() {
            if aqw_diagnostics_enabled()
                && AQW_INVALIDATE_ORIGIN_DEPTH.with(std::cell::Cell::get) == 0
            {
                aqw_note_invalidation(self.into());
            }
            // Don't inform ancestors if we've already done so this frame
            if let Some(parent) = self.parent() {
                AQW_INVALIDATE_ORIGIN_DEPTH.with(|depth| depth.set(depth.get() + 1));
                parent.invalidate_cached_bitmap();
                AQW_INVALIDATE_ORIGIN_DEPTH.with(|depth| depth.set(depth.get() - 1));
            }
        }
    }

    /// Retrieve a named property from the AVM1 object.
    ///
    /// This is required as some boolean properties in AVM1 can in fact hold any value.
    #[no_dynamic]
    fn get_avm1_boolean_property<F>(
        self,
        name: AvmString<'gc>,
        context: &mut UpdateContext<'gc>,
        default: F,
    ) -> bool
    where
        F: FnOnce(&mut UpdateContext<'gc>) -> bool,
    {
        if let Some(object) = self.object1() {
            let mut activation = Activation::from_nothing(
                context,
                Avm1ActivationIdentifier::root("[AVM1 Boolean Property]"),
                self.avm1_root(),
            );
            if let Ok(value) = object.get(name, &mut activation) {
                match value {
                    Avm1Value::Undefined => default(activation.context),
                    _ => value.as_bool(activation.swf_version()),
                }
            } else {
                default(activation.context)
            }
        } else {
            false
        }
    }

    #[no_dynamic]
    fn set_avm1_property(
        self,
        name: AvmString<'gc>,
        value: Avm1Value<'gc>,
        context: &mut UpdateContext<'gc>,
    ) {
        if let Some(object) = self.object1() {
            let mut activation = Activation::from_nothing(
                context,
                Avm1ActivationIdentifier::root("[AVM1 Property Set]"),
                self.avm1_root(),
            );
            let _ = object.set(name, value, &mut activation);
        }
    }

    fn as_drawing(&self) -> Option<RefMut<'_, Drawing>> {
        None
    }

    #[no_dynamic]
    fn as_container(self) -> Option<DisplayObjectContainer<'gc>> {
        match self {
            Self::Avm1Button(dobj) => Some(DisplayObjectContainer::Avm1Button(dobj)),
            Self::LoaderDisplay(dobj) => Some(DisplayObjectContainer::LoaderDisplay(dobj)),
            Self::MovieClip(dobj) => Some(DisplayObjectContainer::MovieClip(dobj)),
            Self::Stage(dobj) => Some(DisplayObjectContainer::Stage(dobj)),
            _ => None,
        }
    }
}

pub enum DisplayObjectPtr {}

macro_rules! impl_downcast_methods {
    ($(
        $vis:vis fn $fn_name:ident for $variant:ident;
    )*) => { $(
        #[doc = concat!("Downcast this display object as a `", stringify!($variant), "`.")]
        #[inline(always)]
        $vis fn $fn_name(self) -> Option<$variant<'gc>> {
            if let Self::$variant(obj) = self {
                Some(obj)
            } else {
                None
            }
        }
    )* }
}

impl<'gc> DisplayObject<'gc> {
    pub fn ptr_eq(a: DisplayObject<'gc>, b: DisplayObject<'gc>) -> bool {
        std::ptr::eq(a.as_ptr(), b.as_ptr())
    }

    pub fn option_ptr_eq(a: Option<DisplayObject<'gc>>, b: Option<DisplayObject<'gc>>) -> bool {
        a.map(|o| o.as_ptr()) == b.map(|o| o.as_ptr())
    }

    impl_downcast_methods! {
        pub fn as_stage for Stage;
        pub fn as_avm1_button for Avm1Button;
        pub fn as_avm2_button for Avm2Button;
        pub fn as_movie_clip for MovieClip;
        pub fn as_edit_text for EditText;
        pub fn as_text_line for TextLine;
        pub fn as_text for Text;
        pub fn as_morph_shape for MorphShape;
        pub fn as_video for Video;
        pub fn as_bitmap for Bitmap;
    }

    pub fn as_interactive(self) -> Option<InteractiveObject<'gc>> {
        match self {
            Self::Avm1Button(dobj) => Some(InteractiveObject::Avm1Button(dobj)),
            Self::Avm2Button(dobj) => Some(InteractiveObject::Avm2Button(dobj)),
            Self::EditText(dobj) => Some(InteractiveObject::EditText(dobj)),
            Self::TextLine(dobj) => Some(InteractiveObject::TextLine(dobj)),
            Self::LoaderDisplay(dobj) => Some(InteractiveObject::LoaderDisplay(dobj)),
            Self::MovieClip(dobj) => Some(InteractiveObject::MovieClip(dobj)),
            Self::Stage(dobj) => Some(InteractiveObject::Stage(dobj)),
            _ => None,
        }
    }

    pub fn downgrade(self) -> DisplayObjectWeak<'gc> {
        match self {
            DisplayObject::MovieClip(mc) => DisplayObjectWeak::MovieClip(mc.downgrade()),
            DisplayObject::LoaderDisplay(l) => DisplayObjectWeak::LoaderDisplay(l.downgrade()),
            DisplayObject::Bitmap(b) => DisplayObjectWeak::Bitmap(b.downgrade()),
            _ => panic!("Downgrade not yet implemented for {self:?}"),
        }
    }
}

bitflags! {
    /// Bit flags used by `DisplayObject`.
    #[derive(Clone, Copy)]
    struct DisplayObjectFlags: u32 {
        /// Whether this object has been removed from the display list.
        /// Necessary in AVM1 to throw away queued actions from removed movie clips.
        const AVM1_REMOVED             = 1 << 0;

        /// If this object is visible (`_visible` property).
        const VISIBLE                  = 1 << 1;

        /// Whether the `_xscale`, `_yscale` and `_rotation` of the object have been calculated and cached.
        const SCALE_ROTATION_CACHED    = 1 << 2;

        /// Whether this object has been transformed by ActionScript.
        /// When this flag is set, changes from SWF `PlaceObject` tags are ignored.
        const TRANSFORMED_BY_SCRIPT    = 1 << 3;

        /// Whether this object has been placed in a container by ActionScript 3.
        /// When this flag is set, changes from SWF `RemoveObject` tags are ignored.
        // TODO [KJ] Can we repurpose it to cover PLACED_BY_AVM1_SCRIPT too?
        const PLACED_BY_AVM2_SCRIPT    = 1 << 4;

        /// Whether this object has been instantiated by a SWF tag.
        /// When this flag is set, attempts to change the object's name from AVM2 throw an exception.
        const INSTANTIATED_BY_TIMELINE = 1 << 5;

        /// Whether this object is a "root", the top-most display object of a loaded SWF or Bitmap.
        /// Used by `MovieClip.getBytesLoaded` in AVM1 and `DisplayObject.root` in AVM2.
        const IS_ROOT                  = 1 << 6;

        /// Whether this object has `_lockroot` set to true, in which case
        /// it becomes the _root of itself and of any children
        const LOCK_ROOT                = 1 << 7;

        /// Whether this object will be cached to bitmap.
        const CACHE_AS_BITMAP          = 1 << 8;

        /// Whether this object has a scroll rectangle applied.
        const HAS_SCROLL_RECT          = 1 << 9;

        /// Whether this object has an explicit name.
        const HAS_EXPLICIT_NAME        = 1 << 10;

        /// Flag set when we should skip running our next 'enterFrame'
        /// for ourself and our children.
        /// This is set for objects constructed from ActionScript,
        /// which are observed to lag behind objects placed by the timeline
        /// (even if they are both placed in the same frame)
        const SKIP_NEXT_ENTER_FRAME    = 1 << 11;

        /// If this object has already had `invalidate_cached_bitmap` called this frame
        const CACHE_INVALIDATED        = 1 << 12;

        /// If this AVM1 object is pending removal (will be removed on the next frame).
        const AVM1_PENDING_REMOVAL     = 1 << 13;

        /// Whether this object has matrix3D (used for stubbing).
        const HAS_MATRIX3D_STUB        = 1 << 14;

        /// Whether this object has been placed by an AVM1 method,
        /// i.e. attachMovie, createEmptyMovieClip, duplicateMovieClip.
        // TODO [KJ] Can this be merged with PLACED_BY_AVM2_SCRIPT?
        const PLACED_BY_AVM1_SCRIPT    = 1 << 15;

        /// Whether this object was placed by the timeline on a `MovieClip`
        /// before the `MovieClip` had its AVM2 object constructed. Such objects
        /// are only instantiated by `Sprite.constructChildren`, which is
        /// usually called when `super()` is called in a `Sprite` subclass.
        /// However, if `super()` (and therefore `Sprite.constructChildren()`)
        /// is never called, the object will never be instantiated. We mark all
        /// objects placed by the timeline on a load frame with this flag to
        /// ensure that `MovieClip::construct_frame` does not instantiate them
        /// (they need to be instantiated "manually" by
        /// `Sprite.constructChildren`).
        const MANUAL_FRAME_CONSTRUCT  = 1 << 16;

        const SUBTREE_NEEDS_FRAME     = 1 << 17;
    }
}

bitflags! {
    /// Defines how hit testing should be performed.
    /// Used for mouse picking and ActionScript's hitTestClip functions.
    #[derive(Clone, Copy)]
    pub struct HitTestOptions: u8 {
        /// Ignore objects used as masks (setMask / clipDepth).
        const SKIP_MASK = 1 << 0;

        /// Ignore objects with the ActionScript's visibility flag turned off.
        const SKIP_INVISIBLE = 1 << 1;

        /// Check only the specified object. Ignore any children of that object.
        const SKIP_CHILDREN = 1 << 2;

        /// The options used for `hitTest` calls in ActionScript.
        const AVM_HIT_TEST = Self::SKIP_MASK.bits();

        /// The options used for mouse picking, such as clicking on buttons.
        const MOUSE_PICK = Self::SKIP_MASK.bits() | Self::SKIP_INVISIBLE.bits();
    }
}

/// A binding from a property of an AVM1 StageObject to an EditText text field.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Avm1TextFieldBinding<'gc> {
    pub text_field: EditText<'gc>,
    pub variable_name: AvmString<'gc>,
}

impl<'gc> Avm1TextFieldBinding<'gc> {
    pub fn bind_variables(activation: &mut Activation<'_, 'gc>) {
        // Check all unbound text fields to see if they apply to this object.
        // TODO: Replace with `Vec::drain_filter` when stable.
        let mut i = 0;
        let mut len = activation.context.unbound_text_fields.len();
        while i < len {
            if activation.context.unbound_text_fields[i]
                .try_bind_text_field_variable(activation, false)
            {
                activation.context.unbound_text_fields.swap_remove(i);
                len -= 1;
            } else {
                i += 1;
            }
        }
    }

    /// Registers a text field variable binding for this stage object.
    /// Whenever a property with the given name is changed, we should change the text in the text field.
    pub fn register_binding(self, dobj: DisplayObject<'gc>, mc: &Mutation<'gc>) {
        if let Some(mut bindings) = dobj.avm1_text_field_bindings_mut(mc) {
            bindings.push(self);
        }
    }

    /// Removes a text field binding for the given text field.
    /// Does not place the text field on the unbound list.
    /// Caller is responsible for placing the text field on the unbound list, if necessary.
    pub fn clear_binding(dobj: DisplayObject<'gc>, text_field: EditText<'gc>, mc: &Mutation<'gc>) {
        if let Some(mut bindings) = dobj.avm1_text_field_bindings_mut(mc) {
            bindings.retain(|b| !DisplayObject::ptr_eq(text_field.into(), b.text_field.into()));
        }
    }

    /// Clears all text field bindings from this stage object, and places the textfields on the unbound list.
    /// This is called when the object is removed from the stage.
    pub fn unregister_bindings(dobj: DisplayObject<'gc>, context: &mut UpdateContext<'gc>) {
        let mc = context.gc();
        if let Some(mut bindings) = dobj.avm1_text_field_bindings_mut(mc) {
            for binding in bindings.drain(..) {
                binding.text_field.clear_bound_display_object(context);
                context.unbound_text_fields.push(binding.text_field);
            }
        }
    }
}

/// Represents the sound transform of sounds played inside a Flash MovieClip.
/// Every value is a percentage (0-100), but out of range values are allowed.
/// In AVM1, this is returned by `Sound.getTransform`.
/// In AVM2, this is returned by `Sprite.soundTransform`.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct SoundTransform {
    pub volume: i32,
    pub left_to_left: i32,
    pub left_to_right: i32,
    pub right_to_left: i32,
    pub right_to_right: i32,
}

impl SoundTransform {
    pub const MAX_VOLUME: i32 = 100;

    /// Applies another SoundTransform on top of this SoundTransform.
    #[must_use]
    pub fn concat(mut self, other: SoundTransform) -> SoundTransform {
        const MAX_VOLUME: i64 = SoundTransform::MAX_VOLUME as i64;

        // It seems like Flash masks the results below to 30-bit integers:
        // * Specifically, 0x40000000, -0x40000000 and -0x80000000 are equivalent to zero.
        // Negative values are equivalent to their absolute value.
        const MASK: i32 = (1 << 30) - 1;

        self.volume =
            (i64::from(self.volume) * i64::from(other.volume) / MAX_VOLUME).abs() as i32 & MASK;

        // This is a 2x2 matrix multiply between the transforms.
        // Done with integer math to match Flash behavior.
        let ll0: i64 = self.left_to_left.into();
        let lr0: i64 = self.left_to_right.into();
        let rl0: i64 = self.right_to_left.into();
        let rr0: i64 = self.right_to_right.into();
        let ll1: i64 = other.left_to_left.into();
        let lr1: i64 = other.left_to_right.into();
        let rl1: i64 = other.right_to_left.into();
        let rr1: i64 = other.right_to_right.into();
        self.left_to_left = ((ll0 * ll1 + rl0 * lr1) / MAX_VOLUME) as i32 & MASK;
        self.left_to_right = ((lr0 * ll1 + rr0 * lr1) / MAX_VOLUME) as i32 & MASK;
        self.right_to_left = ((ll0 * rl1 + rl0 * rr1) / MAX_VOLUME) as i32 & MASK;
        self.right_to_right = ((lr0 * rl1 + rr0 * rr1) / MAX_VOLUME) as i32 & MASK;

        self
    }

    /// Returns the pan of this transform.
    /// -100 is full left and 100 is full right.
    /// This matches the behavior of AVM1 `Sound.getPan()`
    pub fn pan(&self) -> i32 {
        // It's not clear why Flash has the weird `abs` behavior, but this
        // matches the values that Flash returns (see `sound` regression test).
        if self.left_to_left != Self::MAX_VOLUME {
            Self::MAX_VOLUME - self.left_to_left.abs()
        } else {
            self.right_to_right.abs() - Self::MAX_VOLUME
        }
    }

    /// Modifies the pan of this transform.
    /// -100 is full left and 100 is full right.
    /// This matches the behavior of AVM1 `Sound.setPan()`.
    #[must_use]
    pub fn with_pan(mut self, pan: i32) -> SoundTransform {
        if pan >= 0 {
            self.left_to_left = Self::MAX_VOLUME - pan;
            self.right_to_right = Self::MAX_VOLUME;
        } else {
            self.left_to_left = Self::MAX_VOLUME;
            self.right_to_right = Self::MAX_VOLUME + pan;
        }
        self.left_to_right = 0;
        self.right_to_left = 0;
        self
    }

    pub fn from_avm2_object(as3_st: Avm2Object<'_>) -> Self {
        let sound_transform = as3_st
            .as_sound_transform()
            .expect("Should pass SoundTransform");

        SoundTransform {
            left_to_left: (sound_transform.left_to_left() * 100.0) as i32,
            left_to_right: (sound_transform.left_to_right() * 100.0) as i32,
            right_to_left: (sound_transform.right_to_left() * 100.0) as i32,
            right_to_right: (sound_transform.right_to_right() * 100.0) as i32,
            volume: (sound_transform.volume() * 100.0) as i32,
        }
    }

    pub fn into_avm2_object<'gc>(
        self,
        activation: &mut Avm2Activation<'_, 'gc>,
    ) -> Result<Avm2Object<'gc>, Avm2Error<'gc>> {
        let as3_st = activation
            .avm2()
            .classes()
            .soundtransform
            .construct(activation, &[])?
            .as_object()
            .unwrap()
            .as_sound_transform()
            .unwrap();

        as3_st.set_left_to_left(self.left_to_left as f64 / 100.0);
        as3_st.set_left_to_right(self.left_to_right as f64 / 100.0);
        as3_st.set_right_to_left(self.right_to_left as f64 / 100.0);
        as3_st.set_right_to_right(self.right_to_right as f64 / 100.0);
        as3_st.set_volume(self.volume as f64 / 100.0);

        Ok(as3_st.into())
    }
}

impl Default for SoundTransform {
    fn default() -> Self {
        Self {
            volume: 100,
            left_to_left: 100,
            left_to_right: 0,
            right_to_left: 0,
            right_to_right: 100,
        }
    }
}

/// A version of `DisplayObject` that holds weak pointers.
/// Currently, this is only used by orphan handling, so we only
/// need two variants. If other use cases arise, feel free
/// to add more variants.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum DisplayObjectWeak<'gc> {
    MovieClip(MovieClipWeak<'gc>),
    LoaderDisplay(LoaderDisplayWeak<'gc>),
    Bitmap(BitmapWeak<'gc>),
}

impl<'gc> DisplayObjectWeak<'gc> {
    pub fn as_ptr(&self) -> *const DisplayObjectPtr {
        match self {
            DisplayObjectWeak::MovieClip(mc) => mc.as_ptr(),
            DisplayObjectWeak::LoaderDisplay(ld) => ld.as_ptr(),
            DisplayObjectWeak::Bitmap(b) => b.as_ptr(),
        }
    }

    pub fn upgrade(&self, mc: &Mutation<'gc>) -> Option<DisplayObject<'gc>> {
        match self {
            DisplayObjectWeak::MovieClip(movie) => movie.upgrade(mc).map(|m| m.into()),
            DisplayObjectWeak::LoaderDisplay(ld) => ld.upgrade(mc).map(|ld| ld.into()),
            DisplayObjectWeak::Bitmap(b) => b.upgrade(mc).map(|ld| ld.into()),
        }
    }
}

#[cfg(test)]
mod env_flag_tests {
    use super::aqw_env_flag;

    fn with_var(value: Option<&str>, default: bool) -> bool {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "RUFFLE_AQW_ENV_FLAG_TEST";
        match value {
            Some(v) => unsafe { std::env::set_var(NAME, v) },
            None => unsafe { std::env::remove_var(NAME) },
        }
        let result = aqw_env_flag(NAME, default);
        unsafe { std::env::remove_var(NAME) };
        result
    }

    #[test]
    fn unset_returns_the_default() {
        assert!(!with_var(None, false));
        assert!(with_var(None, true));
    }

    #[test]
    fn explicit_false_spellings_turn_the_flag_off() {
        for spelling in ["0", "false", "FALSE", "off", "Off", "no", " false ", "  0"] {
            assert!(
                !with_var(Some(spelling), true),
                "{spelling:?} should read as off"
            );
        }
    }

    #[test]
    fn any_other_value_turns_the_flag_on() {
        for spelling in ["1", "true", "yes", "", " ", "0.0", "00", "falsey"] {
            assert!(
                with_var(Some(spelling), false),
                "{spelling:?} should read as on"
            );
        }
    }
}

#[cfg(test)]
mod url_tests {
    use super::{url_host, url_path_has_segment};

    #[test]
    fn segments_match_whole_and_ignore_case_and_query() {
        assert!(url_path_has_segment(
            "https://game.aq.com/game/gamefiles/Loader3.swf",
            "gamefiles"
        ));
        assert!(url_path_has_segment(
            "https://cdn.example.com/GameFiles/items/Sword.swf",
            "gamefiles"
        ));
        assert!(url_path_has_segment(
            "https://game.aq.com/game/gamefiles/items/Sword.swf?v=2",
            "items"
        ));
        assert!(!url_path_has_segment(
            "https://a/gamefiles2/x.swf",
            "gamefiles"
        ));
        assert!(!url_path_has_segment(
            "https://a/b/x.swf?dir=items",
            "items"
        ));
    }

    #[test]
    fn hosts_drop_the_port_and_the_path() {
        assert_eq!(
            url_host("https://game.aq.com/game/gamefiles/Loader3.swf"),
            Some("game.aq.com")
        );
        assert_eq!(
            url_host("http://localhost:8080/movie.swf"),
            Some("localhost")
        );
        assert_eq!(url_host("relative/movie.swf"), None);
    }
}
