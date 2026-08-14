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

    /// Whether we reported this cache resolving to a zero-sided rect. Separate
    /// from the oversize flag because they are opposite ends of the same
    /// suspicion: a non-finite value crossing into `Twips` collapses one way
    /// and saturates the other, and both were seen in the same object group.
    warned_for_zero: bool,

    /// Render-back offset, relative to the object's translation, that matches
    /// the texture contents from the last admitted redraw. A deferred (stale)
    /// cache must be drawn at this anchor: the live bounds/draw_offset may have
    /// moved or scaled since the texture was rendered, and using them visibly
    /// displaces the object (e.g. a weapon glow drifting away in a busy room).
    stale_anchor: Point<Twips>,

    /// Consecutive rendered frames this cache stayed dirty but had its redraw
    /// deferred by the AQW budget. Feeds the aged-redraw quota so a cache that
    /// keeps losing the budget race (admission is in render order) still
    /// refreshes within about a second instead of staying stale forever.
    deferred_frames: u32,

    /// Consecutive rendered frames this cache was found dirty, whether or not
    /// its redraw was admitted. Distinct from `deferred_frames`, which counts
    /// only refusals and resets on every redraw: this keeps counting through
    /// admitted redraws, so it measures how long the contents have been
    /// *changing* rather than how long they have been stale. See
    /// `aqw_defer_eligible_frames`.
    dirty_streak: u32,

    /// How many times this cache has been baked since the object was created.
    ///
    /// Deliberately survives `clear()`, unlike `dirty_streak`: four paths clear
    /// a cache and every one of them resets that streak, so a cache rebaked
    /// through a clear every frame would read as brand new. This is what
    /// separates the two readings the per-class attribution cannot tell apart --
    /// one object rebaking N times a window, or N objects baking once each --
    /// and those call for opposite fixes.
    bake_count: u32,

    /// Consecutive redraws where the object's transform was unchanged but the
    /// computed cache geometry was not. See `note_static_churn`.
    static_churn: u32,

    /// Whether that churn has been reported, so it is said once per cache.
    churn_reported: bool,
}

/// How long geometry has to keep moving under a static transform before it is
/// reported. An object that resizes once trips this for a frame or two; only
/// a genuine oscillation survives two seconds of it.
const STATIC_CHURN_REPORT_FRAMES: u32 = 48;

const MAX_CACHE_BITMAP_DIMENSION: u32 = 4096;
const MAX_CACHE_BITMAP_PIXELS: u32 = 2_500_000;

/// The ceiling on an AQW cache texture, raised on 2026-08-12 to the one Flash
/// Player itself used: 8191 a side, 16777215 pixels. `RUFFLE_AQW_CACHE_MAX_SIDE`
/// and `RUFFLE_AQW_CACHE_MAX_PIXELS` override; 4096 and 8000000 are the values
/// this fork ran before.
///
/// Refusing a cache is not a graceful degradation here, it is the worst case the
/// renderer has: the subtree renders vector every frame *and* every blend
/// placement inside it becomes its own live pass. Measured 2026-08-12 in
/// Battleon, an avatar item that lost its cache this way (`Dmnk26FiendRider`)
/// emitted ~2490 complex blends per sweep window with the counters repeating to
/// the digit between windows -- static art recomposed 24 times a second -- and
/// took the frame from ~35 ms to ~68. `live_hadcache` was 1222-1922 whenever it
/// was on screen and exactly 0 whenever it was not.
///
/// The old limits were stricter than the player being emulated, and the log said
/// so at every refusal: `flash_acceptable_size=true` beside
/// `aqw_practical_acceptable_size=false`.
///
/// **These are surface pixels, not stage pixels.** The sizes that tripped the
/// old caps -- 4054x2112, 4503x1618 -- are roughly 1750x920 of content times a
/// fullscreen view scale of about 2.3 (window scale times the 1.25 supersample).
/// So the same item kept its cache in a window and lost it in fullscreen, which
/// is the same coordinate-space confusion as the offscreen-bypass defect. The
/// clean fix is to judge content size and let the texture scale with the view;
/// raising the ceiling is the cheap half, and it is the half that can be
/// measured tonight.
/// **Raised to Flash's own limits on 2026-08-12 and put back the same night.**
/// Two sessions with the offending item on screen showed no change at all:
/// `live_hadcache` stayed at 1262-1924 with the ceiling raised, and again with
/// redraw deferral switched off entirely. Whatever suppresses that cache is
/// neither its size nor its admission, so the raise bought nothing and loosened
/// a memory limit in the one area this fork has a standing problem with. Set
/// `RUFFLE_AQW_CACHE_MAX_SIDE=8191` and `RUFFLE_AQW_CACHE_MAX_PIXELS=16777215`
/// to try it again once something says it would help.
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
/// A transform scale component beyond this is degenerate for bitmap caching: even
/// a 1px object would exceed the cache dimension limit, so the cache would be
/// rejected anyway. Used to short-circuit such objects (e.g. AQW's occasional
/// `instance####` with a multi-million-pixel transform) before the bounds
/// traversal and to guard against NaN/inf matrices.
const CACHE_DEGENERATE_SCALE: f32 = MAX_CACHE_BITMAP_DIMENSION as f32;

/// Reads one of the fork's boolean environment switches.
///
/// Presence used to be the whole test, which meant `NO_SCALE9=0` *enabled* the
/// kill switch it reads as disabling -- the one spelling a user is most likely
/// to reach for. An explicit `0`, `false`, `off` or `no` (any case, surrounding
/// whitespace ignored) now turns the switch off; any other value, including an
/// empty one, turns it on. Unset returns `default`.
///
/// Callers cache the result in a `OnceLock`, so this runs once per switch.
pub(crate) fn aqw_env_flag(name: &str, default: bool) -> bool {
    let Some(value) = std::env::var_os(name) else {
        return default;
    };
    let value = value.to_string_lossy();
    let value = value.trim();
    // An empty value means unset. `$env:VAR = ""` is the obvious way to clear a
    // switch in PowerShell but it leaves the variable defined and empty, and
    // reading that as "set" silently keeps the switch on -- which invalidated
    // several field A/Bs on 2026-08-05 before anyone noticed the runs were
    // comparing a lever against itself.
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

/// Kill switch for skipping clean subtrees inside a nested goto frame.
/// `RUFFLE_AQW_NO_FRAME_SKIP=1` restores the unconditional full-stage walk.
pub(crate) fn frame_skip_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_FRAME_SKIP", false))
}

/// Kill switch for handing a nested goto only the orphans that were marked
/// since its last pass. `RUFFLE_AQW_NO_ORPHAN_PENDING=1` puts both orphan loops
/// back on the whole list, every nested frame.
pub(crate) fn orphan_pending_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_ORPHAN_PENDING", false))
}

/// Stop handing a frame script back when it throws out of turn, mid-construction.
/// `RUFFLE_AQW_NO_SCRIPT_REQUEUE=1`.
pub(crate) fn script_requeue_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_SCRIPT_REQUEUE", false))
}
/// Let every dirty cache redraw in the frame it goes dirty, instead of letting
/// the budget defer it and compose the previous texture. **Off by default since
/// 2026-08-12**, when the grace window below made deferral safe enough to keep;
/// `RUFFLE_AQW_NO_REDRAW_DEFER=1` brings it back, and is the escape if wrong
/// colours ever resurface.
///
/// It shipped *on* in V2.8, and that cost 2.4x of frame time in a scene with
/// animated map art (69.9 ms/frame against 29.4 with deferral restored, measured
/// in Battleon on 2026-08-11 with `blend_bake` at 4052 against 267). The
/// paragraph below about the cost is what that measurement corrected: it was
/// taken while standing alone casting one skill, a scene where deferral has
/// almost nothing to defer, and it is wrong by about 27x for a busy one.
///
/// A deferred cache stands in with its last texture, and that texture carries
/// the colour its contents had when it was rendered. For a skill FX whose
/// colour is rewritten once as it is cast, the stand-in is the *old* colour --
/// so the effect flashes between the two while instances take turns being
/// admitted. That is the bug this addresses, and removing deferral is the only
/// thing measured to remove it: four separate confirmations on 2026-08-05,
/// including one unasked-for, when fixing an unrelated env-parsing bug silently
/// re-enabled deferral and the flicker came back with it.
///
/// Bounding how long a cache may stay stale is not enough -- lowering the
/// starvation threshold from 24 frames to 4 shortened each flash and removed
/// none, because a stand-in is the wrong colour on its very first frame.
///
/// Cost, measured in the same session at 1080p fullscreen: submit time per
/// frame went from 6.8-9.8 ms to 8.9-11.1 ms, against a 41.7 ms budget at
/// 24fps. `redraw_large` rose from ~130-200 to ~190-235 per window, which is
/// exactly the deferred work now being done on time.
///
/// What replaced it is not a better budget but a narrower one: deferral is back,
/// and `aqw_defer_eligible_frames` withholds it from caches that have not been
/// changing long enough to be safe to stand in for. Turning this on again
/// disables that too, since there is then no deferral for it to gate.
pub(crate) fn redraw_defer_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_REDRAW_DEFER", false))
}

/// How many frames a cache must have been going dirty in a row before the
/// redraw budget is allowed to defer it. `RUFFLE_AQW_DEFER_ELIGIBLE_FRAMES=N`
/// overrides; `0` restores the ungated budget, which is the behaviour that
/// flickers.
///
/// The bet, and what makes this a different axis from the dead one above: art
/// that changes every frame is *animating*, and one frame of animation lag on
/// it is invisible -- that is the map content the deferral was built for, and
/// where the 2.4x of frame time measured on 2026-08-11 lives. Art that goes
/// dirty after standing still has had something *happen* to it, and a stand-in
/// there shows the state before it happened, which is the skill FX composing
/// its pre-cast colour. So the grace window is not a bound on how stale a
/// texture may get (that was tried, 24 frames to 4, and changed nothing because
/// a stand-in is wrong on its first frame) but on how settled the object has to
/// be before a stale texture is allowed to represent it at all.
///
/// It works only if the colour write lands inside the window, while the clip is
/// young, and that was bisected in the field on 2026-08-12 against the Wicked
/// Purgatory repro in fullscreen: 0 and 4 flicker, 8, 12 and 60 do not. 12 is
/// the default rather than 8 because Wicked Purgatory is one effect and another
/// skill may write its colour later, and because the two are indistinguishable
/// on cost.
///
/// It separates the two populations on its own, with no asset names in it. In
/// Battleon, where the emitters are animated map art, `defer_grace` runs at
/// 0-2% of admitted redraws -- that art never settles, so it stays deferrable
/// and the frame time the budget was restored for is kept. In a room dominated
/// by skill FX, the same counter tracks the effect's blend count roughly 1:1.
///
/// Where it does cost is the transient: entering a busy room builds dozens of
/// caches at once and every one of them is inside its window, which took
/// `defer_grace` to 42-61% of admitted redraws for a window or two (and, at
/// N=60, to 76% with deferral entirely neutralised -- the shape of the
/// 2026-07-30 failure, where an exemption swallowed the budget rather than
/// carving an exception in it). If that transient ever shows up as a hitch on
/// room change, cap graced redraws per frame rather than lowering N: the cap
/// bounds the burst without shortening the window that correctness rests on.
///
/// Steady-state cost was not separable from scene: public-room sessions differ
/// in who is standing in them and what they are wearing, and one heavy item is
/// worth dozens of blend passes per bake, so frame time across runs moved by
/// more than the lever ever did. The honest figure is ~1-2 ms/frame, from the
/// two comparisons that were matched on occupancy and effect load.
pub(crate) fn aqw_defer_eligible_frames() -> u32 {
    static FRAMES: OnceLock<u32> = OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("RUFFLE_AQW_DEFER_ELIGIBLE_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(AQW_DEFER_ELIGIBLE_FRAMES_DEFAULT)
    })
}

/// See `aqw_defer_eligible_frames`. 12 frames is half a second at AQW's 24fps.
const AQW_DEFER_ELIGIBLE_FRAMES_DEFAULT: u32 = 12;

/// Kill-switch for letting a filtered child keep its cache inside an AQW bake,
/// which is the only way its filters get applied at all.
/// `RUFFLE_AQW_NO_NESTED_FILTER_CACHE=1` drops them again.
fn aqw_nested_filter_cache_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_NESTED_FILTER_CACHE", false))
}

/// The per-object flicker probes, which are too expensive to leave on the
/// general diagnostics flag. `RUFFLE_AQW_FLICKER_PROBE=1`.
fn aqw_flicker_probe_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_FLICKER_PROBE", false))
}

/// Restrict the tint probe to objects whose movie URL contains one of these
/// strings, set by giving `RUFFLE_AQW_FLICKER_PROBE` anything other than
/// `1`/`true` (e.g. `RUFFLE_AQW_FLICKER_PROBE=Assets_,IceWolfPuppy`).
/// Comma-separated, so a hunt does not have to know up front which file the art
/// came from.
///
/// Reporting whatever renders first does not survive contact with AQW: measured
/// 2026-08-02, the whole 400-report budget went in 1.4 seconds, 365 of them on
/// `charselect.swf`, before the session reached the skill under investigation.
/// A budget that is reached is a budget that chooses the subject, so the subject
/// is named instead.
fn aqw_flicker_probe_filter() -> Option<&'static str> {
    static FILTER: OnceLock<Option<String>> = OnceLock::new();
    FILTER
        .get_or_init(|| {
            let value = std::env::var("RUFFLE_AQW_FLICKER_PROBE").ok()?;
            (!matches!(value.as_str(), "1" | "true" | "on")).then_some(value)
        })
        .as_deref()
}

/// Whether the probes should look at art from `url`.
///
/// Unfiltered, the character-select screen alone spends a whole report budget
/// before the session is even in a room, so it is never the subject and never
/// gets to be the subject. The game shell and the map are on screen for the
/// whole session and do the same thing more slowly: a run on 2026-08-05 spent
/// 219 of 400 reports on `Game3098*` and `Loader3` and never reached the cast
/// FX.
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

/// True when `url` has `segment` as a whole path segment, ignoring case, the
/// query string and the fragment.
fn url_path_has_segment(url: &str, segment: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .split('/')
        .any(|part| part.eq_ignore_ascii_case(segment))
}

/// Host of an absolute URL, without the port.
fn url_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    Some(authority.split(':').next().unwrap_or(authority))
}

fn is_aqw_movie_url(url: &str) -> bool {
    // Only the `gamefiles` segment is load-bearing: matching the segment rather
    // than the literal `/game/gamefiles/` keeps this working if the path in
    // front of it ever moves.
    url_path_has_segment(url, "gamefiles")
}

/// The last path segment of `url`, for grouping art by file.
fn url_basename(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or(url)
}

/// Tally `items` and render the busiest `limit` as `name:count`.
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

/// Name the orphan list: what these objects are, and how long they have been
/// there.
///
/// The population probe answered "are they inert" (2994 of 2998 neither playing
/// nor holding a queued script) but not "what are they". That gap is what
/// decides the only remaining cheap fix. The freeze is the right shape for this
/// -- it stops the per-frame cost without releasing anything, so it cannot
/// reproduce the #1009 storms that every actual release attempt caused (see the
/// avatar-retention notes) -- but it currently only matches avatar-asset art,
/// and the probe measured just 11 of 3035 orphans matching that. Widening it
/// safely means knowing what the other 3024 are.
///
/// The orphan manager already records the half of this that was known: AQW does
/// not release an avatar mid-session. That explains retention, not the class of
/// object, and the freeze predicate needs the class.
///
/// Once per sweep, so a 3000-entry walk and its strings cost once a second.
fn aqw_report_orphan_census(context: &UpdateContext<'_>) {
    use crate::avm2::object::TObject;

    let orphans = context.orphan_manager.all_orphans(context.gc());
    let total = orphans.len();
    if total == 0 {
        return;
    }

    let (mut clips, mut constructed, mut frozen, mut quiet, mut playing) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    // Of the freezes, how many the widened (structural) predicate accounts for
    // rather than the avatar-art one.
    let mut frozen_inert = 0u64;
    // Ticks on the orphan list, bucketed at AQW's 24fps: under a second, under
    // the freeze's own 5s grace, under 25s, and everything that outlived that.
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

/// Report the URL gates that every game-specific path here hangs off.
///
/// All of them fail closed and silent, so a game update that moved its asset
/// tree would show up only as performance quietly regressing months later. This
/// says it out loud instead, and runs before the gate itself so it still fires
/// when the gate is the thing that broke.
fn aqw_report_gates(context: &mut UpdateContext<'_>) {
    if !aqw_diagnostics_enabled() {
        return;
    }
    static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let frames = FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // ~10s in, once the first room's assets are up, then once a minute.
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

    // Alongside the sweep, for the same reason: the launcher spawns the game
    // detached and discards stdout, so the file is the channel that survives.
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("aqw-memory.log"))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Temporary probe for the detached/uncoloured AQW item art.
///
/// Every item art piece runs, in its first frame,
/// `MovieClip(stage.getChildAt(0)).mcSetColor(this, ...)` — it reaches up to
/// the stage to have the game colour and place it. `mcSetColor` is defined on
/// `Game`, the document class of `Game3097.swf`, and `Loader3.swf` puts that
/// object at index 0 (`stage.removeChildAt(0)` then
/// `stage.addChild(MovieClip(loader.content))`).
///
/// In the field that coercion fails with #1034 against a `Loader`, so the call
/// never happens and the piece keeps its default colour and position. This
/// reports what is actually sitting on the stage, to find out where the
/// `Loader` comes from. Logged at warn so it survives the default `RUST_LOG`.
///
/// Diagnostic scaffolding — remove once the display-list question is answered.
fn aqw_report_stage_children(context: &mut UpdateContext<'_>) {
    let stage = context.stage;

    // Describe index 0 as the item art would find it: the display object kind
    // plus the AVM2 class the coercion would be tested against.
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

    // Watchdog rather than a periodic sample: a snapshot at the character
    // screen showed a MovieClip at index 0, so whatever the failing item art
    // sees is transient and sampling on a timer would miss it.
    //
    // Identity is the child's pointer, compared against the last one seen. This
    // runs every frame, before the sweep's one-per-second throttle, so it must
    // not allocate: resolving the class name and formatting only happen on the
    // frame where index 0 actually changes.
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

    /// Detect a cache that rebuilds every frame while its object stands still.
    ///
    /// `is_dirty` keys off bounds run through `ceil()`, and the draw offset
    /// comes from a filter rect run through `floor()`. An edge that lands on a
    /// pixel boundary can therefore flip between two values indefinitely: the
    /// cache is regenerated each frame, and because the offset is added to the
    /// object's translation at draw time, a flip there paints the art a pixel
    /// away from where it was. Held art that vibrates in place is this.
    ///
    /// Returns the streak length once it is long enough to rule out an object
    /// that merely resized, and only once per cache.
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

    /// Which of `is_dirty`'s conditions fired, for the sweep's `dirty_*`
    /// columns.
    ///
    /// Measured 2026-08-07: restoring the redraw budget cut admitted small
    /// redraws from ~45 to ~5 per frame while `redraw_deferred` stayed at zero.
    /// Deferral cannot do that -- a deferred redraw is a *refused* one and
    /// would show up as deferred -- so the caches were not going dirty in the
    /// first place, and the budget is not what decides that. This says which of
    /// the three conditions is actually firing.
    ///
    /// Ordered by how much each explains, not by how `is_dirty` evaluates them.
    /// A missing texture is the one that would point somewhere other than the
    /// content: nothing the art does removes a texture, but `clear()` on the
    /// offscreen-cache bypass does, and that bypass has a known coordinate-space
    /// defect that makes it fire on art which is plainly on screen.
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
        // `make_dirty` poisons this field, so NaN here means the subtree said it
        // changed rather than that the object moved. Checked before the matrix
        // comparison it would otherwise fall into, and after the two above, so
        // `dirty_notex` and `dirty_size` keep the meaning earlier sessions
        // measured them with and only the `dirty_matrix` bucket splits.
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
            // With size padding, keep an existing texture as long as the
            // logical contents still fit and the texture isn't oversized by
            // more than ~2x in pixels (shrink hysteresis). Pulsing FX bounds
            // then reuse one allocation instead of recreating a GPU texture
            // every frame.
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

        // The flicker probe reports a refused allocation as `has_texture=false`
        // but cannot say why: it runs after `update`, and the sizes that decide
        // this are the post-filter ones, which never leave here. Letting the
        // probe arm this line too spares a run that reads "no texture" with no
        // reason attached -- which is what the 2026-08-06 run was.
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

        // The allocation may be padded up to a size bucket; the logical
        // contents occupy the top-left region and the margin stays transparent
        // (the redraw clears the whole texture), so consumers can keep treating
        // the texture dimensions as the drawable size.
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
        // Nothing survives to stand in, so the object starts over as far as
        // deferral eligibility is concerned.
        self.dirty_streak = 0;
    }

    fn handle(&self) -> Option<BitmapHandle> {
        self.bitmap.as_ref().map(|b| b.handle.clone())
    }

    /// Estimated bytes held by this cache's texture (RGBA), for AQW memory
    /// diagnostics and the cache-memory budget.
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

    /// Whether to apply 9-slice scaling from `scale9Grid`/`DefineScalingGrid`.
    ///
    /// This is disabled only while recursively drawing the individual 9-slice
    /// regions of the object that owns the scaling grid.
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
            // A brand new object always has frame work pending: it has no AVM2
            // object yet, so `construct_frame` has to reach it.
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

    /// Returns whether the value actually changed, so callers can skip work
    /// that only matters on a real change. Content re-applies the same tint
    /// every frame often enough that treating every write as a change is not
    /// affordable.
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

    /// Whether something inside this subtree announced a change this frame.
    ///
    /// Only readable up to `pre_render`, which clears it -- so a descendant that
    /// walks up to ask this always reads false. That is a dead counter waiting
    /// to happen, and the reason the sampling for `live_cand` sits in
    /// `pre_render` itself rather than beside the rest of the blend accounting.
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

/// Indicates which kind of bounds should be returned by `self_bounds`.
/// In most cases `BoundsMode::Engine` should be used.
#[derive(Copy, Clone, Debug)]
pub enum BoundsMode {
    /// The bounds visible on the stage (e.g. takes MorphShape ratio into
    /// account). Used for hit testing and rendering.
    Engine,

    /// The bounds returned by ActionScript (e.g. doesn't take MorphShape
    /// ratio into account - always uses ratio 0 AKA start shape).
    /// This is used in AVM1 in MovieClip::getBounds(), getRect(), _width, _height, hitTest (object)
    /// Used in AVM2 in DO::getBounds(), getRect(), width, height, hitTestObject()
    /// Used in both AVM1 and AVM2 for Transform.pixelBounds.
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
    /// When drawing a stale (deferred) cache, the render-back offset that
    /// matches the texture contents (see `BitmapCache::stale_anchor`), instead
    /// of the one derived from the live bounds/draw_offset.
    offset_override: Option<Point<Twips>>,
    /// This cache was switched on by us for AQW avatar art, not requested by
    /// the content. Such a cache must not inherit `cacheAsBitmap`'s pixel
    /// snapping, and must not be deferred: the object moves, so a stale or
    /// snapped draw is visible as shaking or as art left behind.
    aqw_auto_cache: bool,
}

const SCALING_GRID_EPSILON: f32 = 0.0001;
const AQW_OFFSCREEN_CACHE_LIMIT_PIXELS: f64 = 512.0 * 512.0;
const AQW_OFFSCREEN_CACHE_LIMIT_SIDE: f64 = 1024.0;
const AQW_DIRTY_CACHE_REDRAW_DEFER_MIN_PIXELS: u64 = 16_384;
const AQW_DIRTY_CACHE_REDRAW_DEFER_MIN_SIDE: u32 = 128;
/// Consecutive deferred frames after which a starving cache may claim the
/// aged-redraw quota (~1s at AQW's 24fps).
///
/// Measured in fullscreen during a Wicked Purgatory cast on 2026-08-05:
/// `defer_max` sits pinned at exactly this value while `starved` and
/// `redraw_aged` are both ~3 against a quota of ~96 per window, so every
/// deferred cache rides the threshold to the end and the rescue quota is
/// almost entirely idle. Lowering it to 4 was tried on the strength of that:
/// it bounded the staleness as intended and **did not** fix the wrong colours
/// it was aimed at, while tripling aged redraws. A deferred cache composites
/// the old colour on its very first frame, so shortening the wait shortens
/// each flash and removes none -- duration was never what made it visible.
/// Reverted rather than shipped, since it was cost without a demonstrated
/// benefit.
///
/// `RUFFLE_AQW_STALE_CACHE_FRAMES=N` overrides, for calibrating in the field.
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

/// How far (in twips, per axis) a deferred cache's live offset may drift from
/// its stale anchor before the stale texture stops being a plausible stand-in.
/// Ambient glow pulses move bounds by a few pixels; a weapon swing moves them
/// by hundreds, and anchored old art then shows up visibly detached from the
/// object. 16px covers the former and catches the latter.
const AQW_STALE_ANCHOR_MAX_DRIFT_TWIPS: i32 = 16 * 20;

/// Kill-switch: `RUFFLE_AQW_NO_STALE_ANCHOR` restores the old behavior of
/// drawing deferred caches at the live bounds (the "glow drifts away from the
/// weapon in a busy room" artifact), for field A/B without a rebuild.
fn aqw_stale_anchor_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_STALE_ANCHOR", false))
}

/// Kill-switch: `RUFFLE_AQW_NO_STALE_GUARD` disables the drift guard and
/// always draws deferred caches at their stale anchor, for field A/B.
fn aqw_stale_guard_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_STALE_GUARD", false))
}

/// Kill-switch: `RUFFLE_AQW_NO_DRIFT_NORM` restores the fixed (~1x-calibrated)
/// stale-anchor drift tolerance instead of scaling it by the view, for field
/// A/B without a rebuild.
fn aqw_drift_norm_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_DRIFT_NORM", false))
}

/// Kill-switch: `RUFFLE_AQW_NO_PADDED_CACHE` restores exact-size cache
/// textures, for field A/B without a rebuild.
fn aqw_padded_cache_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_PADDED_CACHE", false))
}

/// Round a cache texture dimension up to a coarse bucket. AQW's animated
/// filtered clips change bounds by a few pixels every frame; allocating the
/// exact size recreates the GPU texture (and every offscreen-pool target
/// derived from it) each frame, and that allocate/destroy churn is what bloats
/// driver memory in busy rooms. Buckets make those small bounds changes land
/// on the same allocation. The padding margin is cleared transparent on every
/// redraw and the on-screen quad covers the whole texture, so the drawn
/// output is unchanged.
fn quantize_cache_dimension(dim: u32) -> u32 {
    if dim <= 1024 {
        dim.next_multiple_of(32)
    } else {
        dim.next_multiple_of(128)
    }
}

/// VRAM pressure level (0 = none, 1 = soft, 2 = hard), updated about once per
/// second by `aqw_cache_sweep` from the renderer's process GPU-memory report.
/// Under pressure the per-frame cache-redraw quotas are clamped (see
/// `Player::render`) so no new cache textures are allocated while the driver
/// drains its deferred-destruction backlog.
static AQW_VRAM_PRESSURE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn aqw_vram_pressure() -> u8 {
    AQW_VRAM_PRESSURE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Kill-switch: `RUFFLE_AQW_NO_VRAM_VALVE` disables the VRAM pressure valve.
fn aqw_vram_valve_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_VRAM_VALVE", false))
}

// The thresholds this valve engages on are shared with the renderer, which
// squeezes its pools off the same numbers; see `ruffle_render::backend` for the
// field calibration behind them.
use ruffle_render::backend::{
    AQW_POOL_HARD_MB, AQW_POOL_HARD_RELEASE_MB, AQW_POOL_SOFT_MB, AQW_POOL_SOFT_RELEASE_MB,
};

/// Refresh `AQW_VRAM_PRESSURE` from the renderer's process GPU-memory report.
///
/// Returns `(used_mb, budget_mb)` for the sweep log (`(0, 0)` if unavailable).
///
/// The valve is driven by how many bytes the offscreen render-target pool is
/// retaining, NOT by the OS video-memory percentage. The DXGI reading is kept
/// for the log only: `CurrentUsage` reports the driver's committed arena, which
/// parks at a constant (measured: 4281 MB for a whole session, unmoved by room
/// changes) while `Budget` fluctuates with unrelated system load. Dividing a
/// parked constant by a noisy denominator made the valve engage on noise, and
/// engaging clamps large redraws to zero - which is what left combat FX stale
/// in ordinary full rooms (measured: `redraw_deferred` 736-1300 and
/// `stale_fallback` 160-430 while engaged, both 0 with the valve released).
/// Pool retention, by contrast, is our own number, responds to actual load, and
/// separates the two regimes cleanly.
///
/// Hysteresis keeps the valve from flapping: the squeeze it triggers lowers the
/// very quantity being measured, so engage and release thresholds are kept a
/// wide band apart.
fn update_aqw_vram_pressure(renderer: &mut dyn RenderBackend) -> (u64, u64) {
    use std::sync::atomic::Ordering;

    if aqw_vram_valve_disabled() {
        AQW_VRAM_PRESSURE.store(0, Ordering::Relaxed);
        return (0, 0);
    }
    // Diagnostics only - see the note above on why this is not the trigger.
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

/// Per-sweep-window counters for the AQW dirty-cache budget, reported by
/// `aqw_cache_sweep` under diagnostics. Relaxed atomics; reset on each sweep.
static AQW_CACHE_REDRAWS_LARGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_REDRAWS_SMALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_REDRAWS_AGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_REDRAWS_DEFERRED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_STALE_FALLBACKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Longest run of consecutive deferred frames any one cache reached in the
/// window: how stale the worst object on screen actually got, in frames.
static AQW_CACHE_DEFER_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Deferrals of caches that were already past `aqw_stale_cache_aged_frames` and
/// still lost -- the backlog queueing for the aged quota. Compared against
/// `redraw_aged`, this says whether the threshold or the quota is the binding
/// constraint, which want different fixes.
static AQW_CACHE_STARVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_LAYERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Avatar-asset subtrees switched to a bitmap cache, cumulative.
pub(crate) static AQW_AVATAR_CACHES_ENABLED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Kill-switch: `RUFFLE_AQW_NO_AVATAR_CACHE` restores live re-rendering of
/// avatar art every frame, for field A/B without a rebuild.
pub(crate) fn aqw_avatar_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_AVATAR_CACHE", false))
}

/// Blend commands tallied by the SWF that emitted them, so a single expensive
/// item can be named rather than inferred.
///
/// The field report is that frame rate tracks *which* items are on screen, not
/// how many players are: one character page in a browser stutters on its own.
/// Attributing blends to their source file is what turns that into a number.
/// Diagnostics-only -- it allocates and locks per blend.
static AQW_BLEND_BY_SWF: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

/// These nine composite through a render pass of their own, and a pass costs a
/// copy of the target plus a read of that copy -- a serialisation point the GPU
/// cannot overlap with anything. The rest fold into fixed-function blend state
/// on an ordinary draw. `blend_layers` counts both populations together, which
/// is why it overstates: 42% of the non-Normal blends in the accumulated sweep
/// are the cheap kind. Mirrors `BlendType::from` in
/// `render/wgpu/src/blend.rs` -- if that mapping moves, this moves with it.
///
/// `Shader` is deliberately not counted here even though it also takes a pass
/// of its own: the renderer tallies it separately from the nine, and the
/// control this feeds (`blend_modes`) is the nine. AQW emits none.
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
    // Just the file name; the directory is the same for every item.
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
        // A room only ever holds so many distinct items; the cap is just to
        // keep a pathological case from growing without bound.
        if counts.len() < 512 || counts.contains_key(name) {
            *counts.entry(name.to_owned()).or_insert(0) += 1;
        }
    }
}

/// Drains the per-SWF tally as `name:count`, busiest first, capped to `limit`.
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

/// Blend attribution, second generation. `blend_swfs` above names the *file* a
/// blend came from, which was enough to convict one item in the V2.0 work but
/// cannot answer the question left open after it: a cached subtree is supposed
/// to collapse its blends into a single texture draw, so the ones still being
/// emitted are either art no cache ever covers, or art whose cache is rebaked
/// so often the cache buys nothing. Those two want opposite fixes and the file
/// name cannot tell them apart -- the same SWF appears on both sides.
///
/// So each blend is booked against the *bake* it happened inside, or against
/// the object itself when it happened live. A bake root that emits dozens
/// means the cost is inside one subtree; many roots emitting one or two each
/// means the cost is how many bakes there are.
///
/// Diagnostics-only: it locks and allocates per blend, and pushes a label per
/// bake.
struct BakeTally {
    /// Bakes started with this object as the outermost cached root.
    bakes: u64,
    /// Complex blends emitted anywhere inside those bakes.
    complex: u64,
    /// Everything else non-Normal, which the renderer folds into blend state.
    trivial: u64,
}

static AQW_BLEND_BY_BAKE: std::sync::Mutex<Option<std::collections::HashMap<String, BakeTally>>> =
    std::sync::Mutex::new(None);
static AQW_BLEND_BY_LIVE: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);
/// The same name for the trivial ones. They were left anonymous while they
/// were assumed to be the cheap half; the renderer-side measurement says they
/// take a full-surface target each and fill 1% of it, so which art emits them
/// is the same question naming `ReignoldKnightCC` answered for the complex
/// ones.
static AQW_BLEND_BY_LIVE_TRIVIAL: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

/// Complex blends split by where they were emitted from. Read against
/// `blend_modes`, whose sum is the renderer's own count of the same passes:
/// `blend_bake + blend_live` has to land on it, or something is emitting
/// blends this attribution never sees and the split below describes a fraction.
static AQW_BLEND_COMPLEX_BAKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_COMPLEX_LIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_TRIVIAL_BAKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_TRIVIAL_LIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Bakes started, and the deepest nesting reached. A cache inside a bake is
/// possible since the filtered-children exemption (2026-08-06), so depth is
/// worth watching: it multiplies the work a single outer bake stands for.
static AQW_BAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BAKE_DEPTH_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// The chain of cache bakes currently being rendered into. Rendering is
    /// single-threaded, and `render_base` pushes on the way into an offscreen
    /// context and pops on the way out, so the bottom of this stack is the
    /// outermost cached root a blend is being emitted underneath.
    static AQW_BAKE_STACK: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `Class@file.swf` -- what a reader needs to find the art. The class name is
/// the useful half (`town-battleon` holds hundreds of distinct clips), the file
/// is what can actually be downloaded and opened.
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

/// Marks the subtree rendered inside `this`'s cache bake, so blends emitted
/// under it are booked against `this`. Pops on drop, which is what keeps the
/// stack balanced through the early returns in the render path.
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
        // Booked at the outermost root, the same place its blends will be, so
        // the ratio in `bake_top` is blends per bake of that root.
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

/// Books one emitted blend. `owner` is the outermost bake root when this blend
/// happened inside a bake, and `None` when it was drawn live.
///
/// Called where the blend is actually handed to the renderer, not where the
/// object's blend mode is read: an object whose subtree drew nothing has its
/// blend thrown away, and counting at the top would book passes that never
/// exist. That gap is a large part of why `blend_layers` reads high.
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
            // Live blends are the population the caching was supposed to
            // remove, so both kinds are worth a name -- the trivial ones cost
            // a target of their own just the same, they only save the pass.
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

/// The bake a blend emitted right now belongs to: the outermost one, since
/// that is the cache whose texture the whole lot ends up inside.
fn aqw_current_bake() -> Option<String> {
    if !aqw_diagnostics_enabled() {
        return None;
    }
    AQW_BAKE_STACK.with(|stack| stack.borrow().first().cloned())
}

/// Every off-screen-cache bypass, and how many of those were on art that was
/// actually on screen. Before the coordinate fix, `bypass_onscreen` was the
/// number that showed the mismatch was live rather than latent: it came back
/// equal to `bypass_all`, every bypass in the session landing on visible art.
///
/// With the fix in place the bypass makes the same comparison this probe does,
/// so `bypass_onscreen` is now a regression guard and belongs at zero. If it
/// climbs again, the two have drifted back apart.
static AQW_CACHE_BYPASS_ALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_CACHE_BYPASS_ONSCREEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Caches the fix kept alive: off-screen by the old comparison, on-screen by
/// the corrected one.
static AQW_CACHE_BYPASS_SAVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BYPASS_BY_OBJECT: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

/// Books one bypass, and decides whether it was justified.
///
/// The bypass compares `bounds` -- which carries the whole render transform
/// stack, viewport matrix included -- against `stage.view_bounds()`, which is
/// set in `stage.rs::build_matrices` as `stage_tx * view_bounds` and so is in
/// stage pixels with no viewport applied. Every other consumer of
/// `view_bounds` in the tree (`graphic.rs`, `bitmap.rs`, `edit_text.rs`,
/// `video.rs`, and our own sweep) compares it against engine-space world
/// bounds. Putting the view rect through `view_matrix()` lands it in the same
/// space `bounds` is already in, which is the cheaper of the two corrections
/// because the bounds traversal has been done by this point.
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

/// Drains the wrongly-bypassed tally as `Class@file:count`, busiest first.
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

/// Whether any ancestor of a live blend carries a bitmap cache.
///
/// This is the other half of the same question, and it needs no probe on the
/// cache side to interpret: if an ancestor had a cache that was engaged this
/// frame, we would either not be rendering at all (clean cache, parent draws
/// its texture) or be inside its bake and booked against it. So a blend
/// arriving here live means no ancestor cache took effect -- and the split is
/// between "no ancestor ever wanted one" and "an ancestor wanted one and it
/// was suppressed", which are different bugs with different fixes.
static AQW_BLEND_LIVE_NO_CACHE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AQW_BLEND_LIVE_HAD_CACHE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// What caching a given ancestor would buy, and what it would cost to buy it.
///
/// Both halves are needed and the fork has twice been tempted to decide on the
/// first alone. A blend pass is not priced by how many there are but by the
/// area each one covers, so two hundred tiny blends can be cheaper left alone
/// than collapsed into a bake that has to cover the screen. `blends` is the
/// prize, `area_px` is the price.
struct LiveAncestorTally {
    /// Live complex blends emitted anywhere under this ancestor.
    blends: u64,
    /// What a bake of this ancestor would have to cover, in stage pixels,
    /// sampled once per window because it costs a bounds traversal.
    area_px: u64,
    /// How far above the emitter this ancestor sits. Shallow and cheap is the
    /// shape worth having; the deepest ancestors are whole maps.
    depth: u32,
}

static AQW_BLEND_LIVE_BY_ANCESTOR: std::sync::Mutex<
    Option<std::collections::HashMap<String, LiveAncestorTally>>,
> = std::sync::Mutex::new(None);

/// Frames in which an object's subtree announced a change, by the same label
/// `live_cand` uses, so the two can be read together.
///
/// This is the half of the price that count-and-area misses. A bake is not paid
/// once: it is paid again every time the contents change, and a candidate that
/// changes every frame charges its whole area every frame. Static art collapses
/// its blends once and lives on the result; animated map art may cost more
/// baked than it did live, which is exactly why "cache the map" has stayed open
/// since 2026-08-08 rather than being tried.
static AQW_CONTENT_DIRTY_FRAMES: std::sync::Mutex<
    Option<std::collections::HashMap<String, u64>>,
> = std::sync::Mutex::new(None);

/// Sampled from `pre_render`, the last point where the invalidation flag is
/// still set. Cheap by construction: a flag read per object per frame, and a map
/// touch only for the ones that actually changed.
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

/// Drains the per-ancestor tally as `Class@file:blends/areaKpx/depth/dirty`,
/// by prize.
///
/// Read `dirty` against `render_frames` on the same line: at `render_frames` the
/// candidate changes every frame and a bake of it recurs in full every frame;
/// near zero it collapses its blends once and the texture then costs nothing.
/// Prize and area alone cannot tell those two apart, and they are the difference
/// between a cache that pays and one that doubles the bill.
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

/// Why an object that asked to be cached ended up drawing itself instead.
///
/// `live_hadcache` says this happened; it cannot say which of six unrelated
/// branches did it, and each one has a different fix. Two hypotheses were spent
/// guessing between them on 2026-08-12 -- the size ceiling and redraw admission,
/// both raised/disabled with the counter unmoved -- before it was cheaper to
/// just record the branch. Same move that split `dirty_matrix`.
#[derive(Copy, Clone)]
pub(crate) enum CacheSuppressReason {
    /// Caching is switched off for this context: the object sits inside an AQW
    /// bake, which disables descendant caches on purpose, and it is not one of
    /// the filtered children exempted from that.
    NotHere,
    /// The offscreen-cache bypass threw it away.
    Bypassed,
    /// Bounds beyond what the cache dimensions can express at all.
    OversizeU16,
    /// The redraw was admitted, `update` ran, and no texture came back with a
    /// non-zero rect to allocate -- so the size gates refused it, or the
    /// renderer did. The `Skipping bitmap cache allocation` line separates
    /// those two, and it fires once per cache.
    AllocRefused,
    /// `update` ran on a rect with a zero side. `NonZero` then refuses the
    /// allocation before any size gate is consulted, so the cache never gets a
    /// texture, the subtree draws itself, and every blend inside it becomes its
    /// own live pass -- for as long as the object exists, since nothing in that
    /// state repairs itself. The detail carries the bounds that produced it.
    AllocZero,
    /// Deferred, with no texture yet to stand in with.
    DeferredNoTexture,
    /// Deferred, holding a texture, but the object has moved too far from where
    /// that texture was rendered for it to be a plausible stand-in.
    DriftFallback,
    /// Not dirty and still no texture, which should not happen.
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

static AQW_CACHE_SUPPRESSED: std::sync::Mutex<
    Option<std::collections::HashMap<String, u64>>,
> = std::sync::Mutex::new(None);

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

/// Drains the suppression tally as `reason|Class@file:count`, busiest first.
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
        // Every ancestor is a candidate: caching any one of them would collapse
        // every blend below it into a single draw. Which one is worth it is the
        // question this answers, and it is deliberately answered for all of
        // them rather than for a hand-picked "root", because every hand-drawn
        // class this fork has written -- avatar asset roots in V2.0 -- has been
        // missed by the next heavy thing the game shipped.
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
    // Only on first sight in this window: a bounds traversal of a whole map
    // subtree is not something to run per blend. Stage pixels, deliberately --
    // the surface size follows from the view scale, and pricing a candidate
    // should not change with the window.
    let bounds = ancestor.world_bounds(BoundsMode::Engine);
    let area_px = (bounds.width().to_pixels().max(0.0) * bounds.height().to_pixels().max(0.0))
        as u64;
    counts.insert(
        label,
        LiveAncestorTally {
            blends: 1,
            area_px,
            depth,
        },
    );
}

/// Drains the per-bake tally as `Class@file:bakes/complex`, busiest first.
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

/// Drains the live-blend tally as `Class@file:complex`, busiest first.
fn take_live_blends(limit: usize) -> Vec<String> {
    take_live_blend_tally(&AQW_BLEND_BY_LIVE, limit)
}

/// The same, for the blends that fold into blend state.
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
/// Frame-tick phase accounting (nanoseconds per sweep window) filled in by
/// `run_all_phases_avm2`, plus how many orphan subtrees the orphan freeze
/// skipped. Splits the tick cost between orphan processing, the on-stage
/// tree, and event broadcasts, so a CPU-bound tick names its hot phase.
pub(crate) static AQW_TICK_ORPHAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_TICK_STAGE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_TICK_BCAST_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHANS_FROZEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Why a bitmap cache was found dirty, for the sweep's `dirty_*` columns.
#[derive(Copy, Clone)]
pub(crate) enum CacheDirtyReason {
    /// The cache holds no texture, so there is nothing to composite and the
    /// redraw is forced. Nothing the content does causes this -- only a
    /// `clear()`, or an allocation that was refused.
    NoTexture,
    /// The subtree's pixel size changed.
    Size,
    /// Something inside the subtree announced that it changed, through
    /// `invalidate_cached_bitmap`. That is the only way a change that leaves
    /// the box and the transform alone -- a colour write, a child swapped for
    /// another of the same size -- can reach the cache at all, since `is_dirty`
    /// compares nothing but geometry. It is recorded by poisoning `matrix_a`
    /// with NaN, so it arrives here indistinguishable from a real transform
    /// change unless it is picked out first; separating the two is the point of
    /// this variant.
    Invalidated,
    /// The composed transform changed: the object moved or scaled, or the
    /// window resized under it.
    Matrix,
}

/// Dirty-cache attribution, plus how many caches the offscreen-cache bypass
/// threw away.
///
/// `dirty_notex` tracking `bypass_cleared` is the signature to look for: a
/// cache cleared and then immediately rebuilt is work the content never asked
/// for, and the bypass that clears it compares two different coordinate spaces
/// (`render_bounds_with_transform`, which carries the viewport matrix, against
/// `stage.view_bounds()`, which does not). At the view scale this fork runs, the
/// rectangle it calls "on screen" is a fraction of the stage, so art in plain
/// view is judged off it.
/// Bakes bucketed by how many times that same cache had already been baked:
/// first ever, 2-4, 5-24, 25-120, and beyond.
///
/// This is the fork the bake investigation turns on, and `bake_top` cannot
/// answer it because it attributes by class. A class showing 37 bakes a window
/// is either one object rebaking almost every frame -- a cache that buys
/// nothing and charges for the privilege, fixable by refusing to cache art that
/// keeps dirtying -- or 37 short-lived objects baking once each, which is
/// honest work and needs a different fix entirely. Mass in the first bucket
/// means the second reading; mass in the last two means the first.
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

/// Drains the repeat tally as `(hist, max)`. The max is a lifetime figure for
/// whichever cache has baked most, so it does not reset with the window --
/// which is what makes a single long-lived offender visible at all.
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
/// The share of what used to be counted as `dirty_matrix` that is really a
/// content change announced from inside the subtree (`CacheDirtyReason::
/// Invalidated`). The two were indistinguishable until 2026-08-12, which is why
/// the plan of "defer geometry, never defer colour" could not be written: the
/// signal it assumed exists in the sweep did not.
pub(crate) static AQW_CACHE_DIRTY_INVALID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Redraws that skipped the budget because the cache had not been dirty long
/// enough to be eligible for deferral (`aqw_defer_eligible_frames`). This is
/// the *added* work the grace window buys, not deferrals it prevented -- the
/// budget might well have admitted them anyway. Read it against `redraw_large`
/// and `redraw_small`: if it approaches their sum, the grace has eaten the
/// budget instead of carving an exception out of it, which is how the
/// 2026-07-30 attempt at exempting colour-dirty caches failed.
pub(crate) static AQW_CACHE_DEFER_GRACE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_CACHE_BYPASS_CLEARED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// What the orphan list is made of, and what a skip could win.
///
/// Measured 2026-08-07: opening the inventory and switching class takes the
/// orphan list from ~105 to 3010 and `tick_orphan_ms` from ~240 to 1720 per
/// ~2s window -- 86% of a core, on a CPU whose single-core speed is AQW's real
/// cap -- and neither number comes back down for the rest of the session. Only
/// ~200 of those orphans are frozen, because the V1.4 freeze only matches
/// avatar art. The history says this is chronic rather than new: past sessions
/// peak at 3000-5700 orphans and 1800-2800ms.
///
/// Two candidate fixes need different facts, so this measures both at once:
///
/// - widening the *freeze* (low risk, the mechanism and its kill-switch exist)
///   needs the population split -- how many orphans are inert and why.
/// - extending the V2.5 subtree mark to the ordinary tick (it is deliberately
///   nested-goto-only, see `can_skip_frame_pass`, because the ordinary frame is
///   the safety net for a mark the scheme fails to set) needs the *prize*:
///   `orph_clean_ms` is the time that a mark-driven skip would not have spent.
///
/// Population counters are per tick, from the Enter pass; the `_ns` pair covers
/// all three passes. Diagnostics-only: one timer pair per orphan visit.
/// How far a cache invalidation travelled to reach the cache it dirtied.
///
/// `invalidate_cached_bitmap` walks up from whatever changed, so a cache is
/// dirtied either by its own geometry or by a descendant some levels down. The
/// two call for opposite fixes and the bake counters cannot tell them apart: a
/// cache that keeps dirtying *itself* is one that should not exist, and the
/// question is whether it pays for its bakes. A cache dirtied from *below* sits
/// above the animation, and no threshold fixes that -- it is placed wrong, and
/// tearing it down only chooses which half of the loss to keep paying.
///
/// Bucketed by distance from the origin: itself, one level, two or three,
/// deeper.
static AQW_INVALIDATE_DEPTH: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Which objects set off invalidations that reached an *ancestor's* cache --
/// "what is animating in there", answered by name rather than by guess.
static AQW_INVALIDATE_ORIGIN: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

thread_local! {
    /// Zero only for the object the invalidation started at, which is the one
    /// worth attributing. Without it the walk below would run again at every
    /// level the recursion climbs.
    static AQW_INVALIDATE_ORIGIN_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Diagnostics-only: walk what the invalidation is about to walk, recording
/// every cache it will dirty and how far up it is.
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

/// Drains the depth histogram as `self/d1/d2-3/d4+`.
fn take_invalidate_depth() -> [u64; 4] {
    use std::sync::atomic::Ordering::Relaxed;
    std::array::from_fn(|i| AQW_INVALIDATE_DEPTH[i].swap(0, Relaxed))
}

/// Drains the origin tally as `Class@file:count`, busiest first.
fn take_invalidate_origins(limit: usize) -> Vec<String> {
    take_live_blend_tally(&AQW_INVALIDATE_ORIGIN, limit)
}

/// Inert probes that ran out of budget before reaching a verdict, and so left
/// their subtree unfrozen however inert it was.
///
/// The direct reading of whether the budget is the binding constraint: it is
/// non-zero exactly when a subtree is too big to judge, and those are the same
/// subtrees whose walk costs the most. Raising `RUFFLE_AQW_INERT_PROBE_BUDGET`
/// should drive this to zero and `orph_clean_ms` down with it; if this stays
/// high after raising it, the budget was not what was stopping the freeze.
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
pub(crate) static AQW_ORPHAN_CLEAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_ORPHAN_DIRTY_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Bill one orphan visit to the clean/dirty split, `clean` being what the V2.5
/// subtree mark says about it.
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

/// AS3 clips whose timeline actually advanced in `enter_frame`, against
/// `aqw_clips` (every clip walked).
///
/// Measured 2026-08-01: `stage_enter_ms` is 98% of the stage tick, and two
/// windows with the same clip count differed 21x in cost -- so the driver is
/// how many clips are *playing*, not how many exist. A frame script that
/// throws abandons everything after it, including the `stop()` that authored
/// timelines almost always end on (see the note on Error #1009), which would
/// leave clips advancing forever. This ratio is what tells those apart.
pub(crate) static AQW_CLIPS_ADVANCED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Nanoseconds inside `run_frame_internal`, the timeline-advance half of
/// `enter_frame`. `stage_enter_ms` minus this is the cost of the tree walk
/// itself -- the recursion, and the queued-tag work every AS3 clip does
/// whether or not it is playing.
///
/// Needed because neither clip count nor playing count predicts the cost:
/// measured 2026-08-01, one window walked 9277 clips with 596 advancing for
/// 124ms, another walked 8079 with 71 advancing for 2253ms.
pub(crate) static AQW_RUNFRAME_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Time in `broadcast_frame_entered`. Nested inside the stage's `enter_frame`,
/// so `stage_enter_ms` has been reporting it as walk cost all along.
pub(crate) static AQW_BCAST_ENTER_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `run_goto` cost, calls, and frames stepped through.
///
/// AQW's aura cooldown handler (`playerAuras.countDownAct`) drives a 90-frame
/// mask with `gotoAndStop` on four segments per aura per frame, and a backward
/// goto restarts at frame 1 and replays forward. Whether that is what makes
/// each handler call cost ~5.4ms, or whether the cost is elsewhere in the
/// handler, is the difference between fixing timeline seeking and fixing
/// something else -- so it gets measured rather than assumed.
pub(crate) static AQW_GOTO_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `run_goto` split into its three phases, plus placement tags parsed.
///
/// A goto costs ~1.4ms while stepping 12-17 frames -- about 100us per frame,
/// far more than reading a handful of tags should take. The phases have very
/// different fixes: scanning is tag parsing (a `PlaceObject3` parse allocates
/// for filters and name), removal walks the render list, and applying
/// instantiates or updates children. Measure before rewriting any of them.
pub(crate) static AQW_GOTO_SCAN_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_REMOVE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_APPLY_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_GOTO_TAGS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The apply phase split again: placing children (`run_goto_command` over the
/// merged commands) against `run_inner_goto_frame`, the recursive frame an
/// explicit goto runs afterwards.
///
/// Apply is 98% of a goto -- scanning tags is 1.7% -- so this is the cut that
/// says whether the cost is placing objects or re-running a whole frame.
/// Gotos that skipped the recursive frame because they changed nothing that
/// needs one.
pub(crate) static AQW_GOTO_FAST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Opt-in, and off by default because it is **not** semantically correct.
///
/// Skipping the recursive frame breaks 10 timeline tests
/// (`movieclip_gotoandstop_queueing`, `movieclip_displayevents_*`,
/// `swf_10_queued_goto_scripts_construct`, ...): the frame is a *global*
/// operation -- it broadcasts `frameConstructed`/`exitFrame`, runs other
/// clips' frame scripts, and drains queued tags -- so "this clip changed
/// nothing structural" is not a sound reason to skip it.
///
/// Kept behind `RUFFLE_AQW_GOTO_FASTPATH=1` only to measure the ceiling: it
/// takes an aura goto from 6600us to roughly what SWF 9 content pays. The
/// real fix is to make the recursive frame cheap rather than to skip it.
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

/// Broadcast-handler time by listener class, for calls over 0.1ms.
static AQW_LISTENER_COST: std::sync::Mutex<Option<std::collections::HashMap<String, (u64, u64)>>> =
    std::sync::Mutex::new(None);

/// Record one handler call. `(calls, nanoseconds)` per class.
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

/// Top `n` listener classes since the last call, as `Class=ms/calls`.
fn aqw_listener_cost_top(n: usize) -> String {
    let Ok(mut guard) = AQW_LISTENER_COST.lock() else {
        return String::new();
    };
    let Some(map) = guard.as_mut() else {
        return String::new();
    };
    // Microseconds, so a handler that is cheap per call but runs constantly is
    // still visible instead of rounding to zero.
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

/// `run_inner_goto_frame_impl` split into its steps.
///
/// The subtree skip took `gotoAndStop` from ~1590us to ~500us per call, but it
/// only covers the two stage walks. The nested frame also iterates every orphan
/// twice and runs `cleanup_dead_orphans`, none of which the skip touches, and at
/// ~2000 nested frames per window against 99-1755 orphans that is a plausible
/// remainder. Measure which step it actually is before changing any of them.
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

/// The orphan loops are 70-77% of the nested frame, but the cost barely moves
/// between 85 and 2943 orphans, so it is not obviously proportional to the list.
/// These separate the two candidates: `VISITS` counts every orphan the loops
/// hand to the callback, `WORK` counts the ones that were actually dirty. If
/// visits dominate, the fix is to stop iterating; if work does, the fix is to
/// stop those orphans from staying dirty.
/// Why the orphan gate reopens. It engages while the orphan list is small but
/// stops working once the list balloons (measured: 1490 of 1514 nested frames
/// found it open at 1719 orphans), so something bumps the epoch on nearly every
/// goto. The two candidates need opposite fixes: `ORPHAN_ADDS` means gotos are
/// orphaning children, `MARK_BUMPS` means marks are terminating outside the
/// stage tree.
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

/// The queued-tag half of `enter_frame`, which `runframe_ms` does not cover:
/// draining the pending `PlaceObject` queue (`unqueue_ms`) and executing those
/// tags (`place_ms`, `places` = tags run).
///
/// This is the last unmeasured block in the phase. Everything else has been
/// ruled out by measurement: timeline advance is ~5ms, render-list copies run
/// 3-15 per frame at one element each, and the fork's own hook is 2%.
pub(crate) static AQW_UNQUEUE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_PLACE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_PLACE_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Render-list deep copies forced by `Rc::make_mut`, and elements copied.
///
/// `enter_frame` keeps a `RenderIter` alive at every level while it recurses,
/// and each one holds a strong `Rc` to that container's render list. Any child
/// added or removed while the walk is in flight therefore clones the whole
/// list instead of mutating it in place -- by design, so iteration stays valid
/// (see the comment on `ChildContainer::render_list`), but the cost lands on
/// exactly the workload that churns children fastest.
///
/// Measured 2026-08-01: walking a constant ~8800 clips cost 0.12us/clip at
/// rest and 11.19us/clip during skill spam, with timeline advance flat at
/// ~2ms. These two counters are what tie that 100x to the copies.
pub(crate) static AQW_RENDERLIST_COPIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_RENDERLIST_COPIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `tick_stage_ms` split by phase.
///
/// The three stage passes cost very different things -- advancing timelines,
/// constructing newly placed objects, and running the content's own frame
/// scripts -- and summing them hides which one grew. Measured 2026-08-01: the
/// total is 42-58ms/frame on the updated game while the fork's own per-clip
/// hook accounts for ~1.2ms of it, so the answer is in here.
pub(crate) static AQW_STAGE_ENTER_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_STAGE_CTOR_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_STAGE_SCRIPT_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Clips the per-clip AQW timeline hook ran on, and nanoseconds it spent, per
/// sweep window.
///
/// `enter_frame` calls that hook on every `MovieClip` in the tree, so its cost
/// scales with the size of the display list rather than with anything AQW-
/// specific. The counters separate it from the rest of the stage phases, which
/// `tick_stage_ms` lumps together with the game's own frame scripts -- without
/// them there is no way to tell a hook that got expensive from content that
/// got heavier.
pub(crate) static AQW_HOOK_CLIPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static AQW_HOOK_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Kill-switch for the per-clip AQW timeline hook, so its whole cost can be
/// taken out in the field for an A/B without a rebuild. Turning it off also
/// turns off the avatar cache and both freezes, so compare `tick_stage_ms`
/// (CPU) rather than frame rate.
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

/// Test toggle: when `RUFFLE_AQW_NO_SCALE9` is set, the 9-slice scaling path is
/// disabled, so we can measure its FPS cost against the visual fix it provides.
fn aqw_scale9_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_SCALE9", false))
}

thread_local! {
    /// When the stage viewport last changed size (a window resize). Set from
    /// `Stage::build_matrices`; read by the 9-slice path below.
    static LAST_AQW_RESIZE: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
}

/// How long after a resize the AQW 9-slice path stays disabled. A resize
/// invalidates every bitmap cache at once; in a crowded room the per-frame
/// redraw budget can't refresh them all immediately, and AQW simultaneously
/// re-lays-out its HUD by real pixels. During that storm the 9-slice path can
/// paint a panel (e.g. the gold/exp bar) black, and a deferred parent cache
/// freezes that black frame for many frames. Falling back to the normal render
/// path for a moment after a resize avoids the black (the normal path degrades
/// to stale-but-visible, which is what `RUFFLE_AQW_NO_SCALE9` showed); 9-slice
/// resumes once the layout settles. Resizes don't happen in normal play, so
/// this has no steady-state cost.
const AQW_RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);

/// Record that the stage viewport just changed size. Called from
/// `Stage::build_matrices` when the stage size actually changes.
pub fn note_aqw_stage_resize() {
    LAST_AQW_RESIZE.with(|cell| cell.set(Some(std::time::Instant::now())));
}

/// Whether we're still within the post-resize settle window.
///
/// This is called for every display object every frame (via `render_base`),
/// so the common case - no resize in the last second - must be nearly free:
/// it's a single thread-local read of `None`. Only within the settle window
/// after an actual resize do we pay the `Instant::elapsed()` timer read, and
/// once the window elapses we reset the cell back to `None` so subsequent
/// frames take the cheap path again. (Reading a timer per object per frame,
/// as an earlier version did, measurably cost FPS in effect-heavy boss fights
/// on slower `QueryPerformanceCounter` hardware.)
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
    // Ordered cheapest-first. This runs for every display object on every
    // frame, and virtually none of them have a scaling grid, so the tests that
    // reject them should be the free ones: `options` is `Copy` and the grid is
    // a `Cell` read, while the URL test clones an `Arc` and scans a string.
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

    // AQW window panels use an object-local scale and translation. Preserve
    // their corners and edges while stretching only the scaling-grid center.
    // Rotated or skewed content keeps the standard rendering path.
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
    // Each slice draws the subtree under its own matrix, so a cache down there
    // is dirty on nearly every one of the nine and would be baked -- and often
    // reallocated -- nine times a frame. That is why this path clears the flag
    // above, and the filter exemption has to be cleared with it: a panel drawn
    // inside a bake arrives here with it set.
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

    // Ordered ahead of the on-screen test only so the cheap arithmetic settles
    // first and the two comparisons below are done once; both are pure
    // predicates and this object is bypassed exactly when both agree.
    if width * height < AQW_OFFSCREEN_CACHE_LIMIT_PIXELS
        && width < AQW_OFFSCREEN_CACHE_LIMIT_SIDE
        && height < AQW_OFFSCREEN_CACHE_LIMIT_SIDE
    {
        return false;
    }

    // The two sides of this comparison were in different coordinate spaces.
    // `bounds` comes from `render_bounds_with_transform` over the render
    // transform stack, so it carries the viewport matrix -- supersampling times
    // window scale, measured at 2.29 in a large window. `stage.view_bounds()`
    // is set in `stage.rs::build_matrices` as `stage_tx * view_bounds`, from
    // `movie_width`/`movie_height` in stage pixels, with no viewport in it.
    //
    // The effect was not a wash: at view scale 2.29 the rectangle art had to
    // reach to be considered visible shrank to ~44% of the stage per axis, so
    // art plainly on screen to the right or below was judged off-screen and had
    // its cache thrown away. It costs no correctness -- the bypass only falls
    // back to vector rendering, which draws the same picture -- but a subtree
    // that loses its cache re-emits every blend mode inside it, every frame.
    // Measured in the field: one wrong bypass per frame on a weapon released 36
    // complex blend passes per frame, 19% of the passes in the heaviest window.
    //
    // Putting the view rect through `view_matrix()` lands it in the space
    // `bounds` is already expressed in. The other direction -- comparing
    // `world_bounds(BoundsMode::Engine)` against `view_bounds` untouched -- is
    // what the five other consumers of `view_bounds` do (`graphic.rs`,
    // `bitmap.rs`, `edit_text.rs`, `video.rs`, and our own sweep) and is equally
    // correct, but it costs a second bounds traversal where this costs a matrix
    // multiply on a rectangle already to hand.
    //
    // Kill-switch: `RUFFLE_AQW_NO_BYPASS_VIEW_FIX=1` restores the mismatched
    // comparison, for field A/B without a rebuild.
    let stage_view_bounds = context.stage.view_bounds();
    let on_screen = if aqw_bypass_view_fix_disabled() {
        bounds.intersects(&stage_view_bounds)
    } else {
        bounds.intersects(&(context.stage.view_matrix() * stage_view_bounds))
    };

    // How often the fix is the deciding vote: the old comparison called this
    // off-screen and the corrected one does not, so a cache that used to be
    // discarded now survives. This is the number that says whether a field A/B
    // is measuring anything.
    if aqw_diagnostics_enabled() && on_screen && !bounds.intersects(&stage_view_bounds) {
        AQW_CACHE_BYPASS_SAVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    !on_screen
}

/// Kill-switch: `RUFFLE_AQW_NO_BYPASS_VIEW_FIX` restores the stage-space vs
/// render-space comparison the off-screen cache bypass used to make.
fn aqw_bypass_view_fix_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| aqw_env_flag("RUFFLE_AQW_NO_BYPASS_VIEW_FIX", false))
}

/// Total bitmap-cache memory budget in MB before off-screen caches are evicted.
/// Controlled by `RUFFLE_AQW_CACHE_BUDGET_MB`; `0`/unset disables eviction.
fn aqw_cache_budget_mb() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("RUFFLE_AQW_CACHE_BUDGET_MB")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0)
    })
}

/// Once-per-second AQW memory sweep. Totals bitmap-cache memory across the
/// display tree and, with `RUFFLE_AQW_DIAGNOSTICS`, logs orphan/loader/cache
/// counts so the source of a leak can be identified. When cache memory exceeds
/// `RUFFLE_AQW_CACHE_BUDGET_MB`, off-screen caches are evicted to cap memory.
/// No-op unless diagnostics or a budget are enabled.
pub fn aqw_cache_sweep(context: &mut UpdateContext<'_>) {
    let diagnostics = aqw_diagnostics_enabled();
    let budget_mb = aqw_cache_budget_mb();
    aqw_report_gates(context);
    // Before the AQW gate: the probe has to report even if the root movie is
    // not what we expect, since that is one of the things it could reveal.
    aqw_report_stage_children(context);
    if !is_aqw_movie_url(context.stage.movie().url()) {
        return;
    }

    // Throttle to roughly once per second.
    static SWEEP_FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if !SWEEP_FRAME
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .is_multiple_of(48)
    {
        return;
    }

    // VRAM pressure valve: sample the renderer's GPU-memory report and adjust
    // the per-frame cache-redraw quotas (consumed in `Player::render`). This
    // runs even with diagnostics off - it's the field guard against the
    // paging collapse when the driver's texture backlog fills the card.
    let (vram_mb, vram_budget_mb) = update_aqw_vram_pressure(context.renderer);

    if !diagnostics && budget_mb == 0 {
        return;
    }

    // Evict using the previous sweep's total so this stays a single pass.
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
        // Probe for a suspected leak: `MovieLibraries` is keyed by `Weak<SwfMovie>`
        // but `MovieLibrary` also stores a strong `Arc<SwfMovie>` (self.swf), so the
        // weak key can never expire and entries accumulate forever. Count total
        // libraries known, plus how many are AQW asset movies (item/avatar/map
        // SWFs loaded via `/game/gamefiles/`), to see if this grows monotonically
        // as items/avatars are loaded over a session.
        let movie_libs_total = context.library.known_movies().count();
        let movie_libs_aqw = context
            .library
            .known_movies()
            .filter(|m| is_aqw_movie_url(m.url()))
            .count();
        // Probe: how many topmost avatar-asset clips (armor pieces, helms,
        // capes, weapons, hair — roughly 8-14 per avatar) the timeline
        // throttle counted last frame. Frozen (hidden/TRASH) subtrees are
        // excluded. The crowded-room throttle engages at
        // `AQW_AVATAR_THROTTLE_ROOTS` (96) and hardens at 192.
        let avatar_roots = *context.aqw_avatar_asset_roots_previous;
        // Dirty-cache budget activity accumulated since the previous sweep
        // (~48 frames): how many redraws were admitted per class, how many were
        // deferred, and how many blend layers were pushed. `redraw_small` and
        // `blend_layers` are the FX-storm signature (fireworks, ultra skill spam).
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
        // Population of the orphan list (per tick, so divide by `render_frames`
        // for a headcount) and the clean/dirty cost split across all three
        // passes. `orph_clean_ms` against `tick_orphan_ms` is the ceiling on
        // what a mark-driven skip in the ordinary tick could win.
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
        // `self` against the rest is the fork: a cache dirtying itself is a
        // cache that may not pay for its bakes; a cache dirtied from below is a
        // cache sitting above the animation. `dirty_origin` names what does the
        // dirtying when it comes from below.
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
        // Read against `tick_stage_ms`: the share of the stage phases that is
        // our per-clip hook rather than AQW's own scripts. `aqw_clips` is the
        // display-list size it walks, which is what a content update moves.
        let aqw_hook_ms = AQW_HOOK_NS.swap(0, Ordering::Relaxed) / 1_000_000;
        let aqw_clips = AQW_HOOK_CLIPS.swap(0, Ordering::Relaxed);
        // These three sum to `tick_stage_ms`.
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
        // Offscreen texture-pool churn since the previous sweep (~48 frames):
        // `pool_allocs` = new GPU textures the pool had to create (misses),
        // `pool_free` = idle ones maintenance destroyed, `pool_mb` = bytes
        // currently retained for reuse. Sustained `pool_allocs` is the
        // deferred-destruction churn (§5 commit creep) that the padded
        // filter targets and age-based eviction are meant to kill.
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
        // Same three numbers for the main-surface pool. That pool feeds the
        // scene draw and every blend/mask/filter target nested in it, whose
        // sizes follow on-screen content — so a crowded room asks it for many
        // distinct sizes. It is reported separately because the columns above
        // cover only the offscreen pool, which can look idle while this one
        // grows.
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
        // What shape the surface-pool retention has. A crowded room where two
        // particular players dominate the cost points at a few oversized
        // filter/blend targets rather than sheer count, and only the size
        // breakdown can tell those apart.
        let tex_top = context
            .renderer
            .surface_pool_largest(4)
            .iter()
            .map(|(w, h, count, bytes)| format!("{w}x{h}x{count}={}MB", bytes / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(",");

        // Which blend modes are paying for their own full-surface pass. The
        // `blend_layers` count above says how many there were; this says which,
        // and that decides whether there is a cheap fix (modes expressible as
        // GPU blend state) or only the structural one.
        let blend_modes = context
            .renderer
            .take_complex_blend_counts()
            .iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(",");

        // How much of the surface those passes actually covered. The counts
        // above cannot show this: bounding a blend pass removes no passes, it
        // shrinks them, so `blend_layers`/`blend_modes` stay put either way and
        // only `blend_cover` moves. `blend_cover_hist` is the per-layer spread
        // (<=1%, <=5%, <=25%, >25%), so a few full-screen overlays cannot hide
        // a good median.
        // `blend_alloc` is the memory side: how big those targets were, against
        // full-surface ones. Hundreds are alive at once in a crowded room, so
        // this is what decides whether VRAM clears the OS grant.
        let blend_alloc = context.renderer.take_blend_alloc();
        // How much of the multiply population -- 55% of complex blends in a
        // crowded room -- composites onto an opaque destination, where
        // fixed-function blend state is exactly equivalent and the pass of its
        // own could collapse into the batched draw. Reads as
        // `onto_opaque/total (Npx)`; the megapixels are the scale, since the
        // same ratio over tiny targets is not the same prize. A low ratio
        // closes the question, and closing it is the point.
        let (blend_mul_opaque, blend_mul_total, blend_mul_opaque_mpx) =
            context.renderer.take_blend_dest_opacity();
        let blend_mul_opaque = format!(
            "{blend_mul_opaque}/{blend_mul_total} ({blend_mul_opaque_mpx}Mpx)"
        );
        // Where the frame actually goes. `render_frames` is how many frames the
        // other two cover, so they read as ms/frame against the 41.7ms budget
        // at 24fps. `commit_mb` is system memory, which hit 98% of the machine
        // while VRAM sat well inside its budget -- they run out separately.
        let (render_encode_ms, render_submit_ms, render_frames, commit_mb) =
            context.renderer.take_render_timings();
        // Which files those blends came from. If one item dominates, the cost
        // is that item's art, not the number of players -- which is what the
        // field reports and what a stuttering single-character page implies.
        let avatar_caches = AQW_AVATAR_CACHES_ENABLED.load(Ordering::Relaxed);
        let blend_swfs = take_blend_sources(6)
            .iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(",");
        // Where the expensive blends come from. `blend_bake`+`blend_live` is the
        // control against `blend_modes`, whose sum is the renderer's own count of
        // the same passes; if they disagree, this attribution is missing a path.
        // Two known reasons for a small shortfall the other way, where the
        // renderer counts fewer: it drops an Alpha/Erase with no Layer above it
        // and one whose scissor came out empty. One reason it counts more:
        // `BitmapData.draw` blends without going through here, which is AQW's
        // own rasteriser (Static Player Art, the map raster) rather than the
        // scene, and is worth knowing about separately if the gap is large.
        //
        // The split is the question: a cached subtree is meant to collapse its
        // blends into one texture draw, so `blend_live` is art no cache covers,
        // and `blend_bake` is art whose cache is being rebuilt often enough that
        // the cache buys nothing. `bake_top` reads `Class@file:bakes/complex` --
        // dividing the two says whether one bake is expensive or there are simply
        // a lot of them, which is the fork the whole investigation turns on.
        let blend_bake = AQW_BLEND_COMPLEX_BAKED.swap(0, Ordering::Relaxed);
        let blend_live = AQW_BLEND_COMPLEX_LIVE.swap(0, Ordering::Relaxed);
        let blend_bake_triv = AQW_BLEND_TRIVIAL_BAKED.swap(0, Ordering::Relaxed);
        let blend_live_triv = AQW_BLEND_TRIVIAL_LIVE.swap(0, Ordering::Relaxed);
        let bakes = AQW_BAKES.swap(0, Ordering::Relaxed);
        // Reads as `first/2-4/5-24/25-120/more`. Against `bake_top`, which is
        // per class, this says whether that class is one object rebaking or a
        // stream of new ones.
        let (bake_reps, bake_reps_max) = take_bake_repeats();
        let bake_reps = bake_reps
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("/");
        let bake_depth = AQW_BAKE_DEPTH_MAX.swap(0, Ordering::Relaxed);
        let bake_top = take_bake_blends(6).join(",");
        let blend_live_top = take_live_blends(6).join(",");
        // The two halves of "why is this blend not inside a cache".
        // `bypass_all` against `bypass_cleared` is the correction to how that
        // counter reads; `bypass_onscreen` is the defect firing on art the
        // player can see, and `bypass_top` names it.
        // `live_nocache`/`live_hadcache` splits the live blends themselves:
        // no ancestor ever asked for a cache, against an ancestor asked and
        // did not get one.
        let bypass_all = AQW_CACHE_BYPASS_ALL.swap(0, Ordering::Relaxed);
        let bypass_onscreen = AQW_CACHE_BYPASS_ONSCREEN.swap(0, Ordering::Relaxed);
        let bypass_saved = AQW_CACHE_BYPASS_SAVED.swap(0, Ordering::Relaxed);
        let bypass_top = take_bypassed_objects(6).join(",");
        let live_nocache = AQW_BLEND_LIVE_NO_CACHE.swap(0, Ordering::Relaxed);
        let live_hadcache = AQW_BLEND_LIVE_HAD_CACHE.swap(0, Ordering::Relaxed);
        // `Class@file:blends/areaKpx/depth` -- what caching that ancestor would
        // collapse, against what its bake would have to cover. Divide blends by
        // `render_frames` for a per-frame prize. A candidate worth having is a
        // large first number beside a small second one; the map root will show
        // the largest prize of all and is worthless, because its area is the
        // whole scene.
        let live_cand = take_live_ancestors(8).join(",");
        // `reason|Class@file:count` -- which branch abandoned a cache the
        // content (or the auto-cache) had asked for. This is the column that
        // says what to fix when `live_hadcache` is high.
        let cache_suppress = take_cache_suppressed(8).join(",");
        // `Dictionary` retention. The class takes a `weakKeys` flag and drops
        // it, so object-space entries are pinned for the dictionary's whole
        // life; content that expects the player to drop unreachable keys gets
        // a leak instead. `dict_okeys` is the size of that exposure -- string
        // and numeric keys are plain dynamic properties, so they cannot be
        // weak and are deliberately not counted here.
        let (dicts, dict_okeys) = crate::avm2::object::dictionary_stats();
        // The third texture bucket, and the only one nothing else reports:
        // everything held by a `BitmapHandle`. `cache_mb` above walks the
        // stage, so a cache on a detached subtree -- an orphaned avatar, a
        // clip parked in AQW's TRASH -- stops being counted while its texture
        // stays alive. `bmp_mb` counts it regardless of where the owner sits.
        let (bmp_tex, bmp_mb) = context
            .renderer
            .bitmap_texture_stats()
            .map(|(count, bytes)| (count, bytes / (1024 * 1024)))
            .unwrap_or((0, 0));
        // Same breakdown `tex_top` gives the surface pool. A room-sized bucket
        // repeated hundreds of times says the map raster; thousands of small
        // ones say avatar art. The totals cannot tell those apart.
        let bmp_top = context
            .renderer
            .bitmap_texture_largest(15)
            .iter()
            .map(|(w, h, count, bytes)| format!("{w}x{h}x{count}={}MB", bytes / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(",");
        // How many distinct sizes those textures come in, and the total across
        // all of them. `bmp_sizes` separates "a few allocations repeated" from
        // "the same content re-rasterized at dimensions that keep shifting",
        // and `bmp_tracked_mb` is the control: it has to land near `bmp_mb`,
        // or the breakdown above is describing only a fraction of the problem.
        let (bmp_sizes, bmp_tracked) = context.renderer.bitmap_texture_buckets();
        let bmp_tracked_mb = bmp_tracked / (1024 * 1024);
        let (blend_cover, blend_cover_buckets) = context.renderer.take_blend_coverage();
        let blend_cover_hist = blend_cover_buckets
            .iter()
            .map(|count| count.to_string())
            .collect::<Vec<_>>()
            .join("/");

        // What used to be the blind spot: everything above counts only the
        // blends that cost a pass of their own, and a trivial blend skips the
        // pass and nothing else -- it takes the same target, clear, sub-render
        // and composite quad. Until this was measured, neither the V2.0 scissor
        // nor the target shrink reached them, because both were gated on the
        // blend being complex.
        //
        // `blend_triv_cover` is the same question `blend_cover` answers for the
        // complex ones (how much of the target was ever written) and
        // `blend_triv_alloc` mirrors `blend_alloc` (how big it was against a
        // full-surface one). They now move in opposite directions, and that is
        // the check that the sizing is working: alloc should be small and cover
        // large. Before it, alloc was 100% by construction and cover read 0-1%.
        // `blend_triv_mpx` is the absolute scale both have to be read against;
        // divide it by `render_frames` for per frame, where the same empty room
        // measured 109.7 before and is the number to compare against.
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

        // Built once, emitted twice. The two destinations used to carry their
        // own field lists and had already drifted ~30 columns apart -- the
        // console was missing every `goto_*`, `inner_*` and `stage_*` the file
        // had. Nothing checks two hand-written lists of eighty names against
        // each other, so there is deliberately only one.
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
             orph_clean_ms={orph_clean_ms} orph_dirty_ms={orph_dirty_ms} \
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

        // Also append to a file so the sweep is captured even when the game is
        // spawned detached (the normal launcher discards stdout/stderr).
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

/// Ping-pong state for one display object, keyed by pointer in
/// `note_position_oscillation`.
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

/// Report art that vibrates in place, whatever kind of object it is.
///
/// Earlier probes watched one mechanism each — cache geometry, then bitmap
/// pixel snapping — and a vector `Graphic` would have tripped neither. This
/// watches the only thing all of them share: the world position the object is
/// actually drawn at.
///
/// The signature is a strict A-B-A ping-pong of small amplitude, sustained.
/// Real animation walks through many positions; alternating between exactly
/// two, a fraction of a pixel apart, for dozens of frames is an artifact. What
/// it cannot say on its own is *whose* artifact — but the amplitude does: an
/// exact 1.0 means something is rounding, anything else means the transform
/// itself is being driven that way.
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

/// List map-art clips whose timeline keeps running.
///
/// With the position probe silent, art that still appears to move has to be
/// changing what it *draws* rather than where: a clip advancing between frames
/// whose contents sit at different heights looks exactly like the object
/// bobbing. The interesting case is a clip that is stopped in the player and
/// running here, which no rendering fix would ever reach.
///
/// Reports name, class and frame count, because those are what identify a
/// piece of scenery — every probe so far could see that *something* was wrong
/// without being able to say which object it was.
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

/// What `note_tint_progression` accumulates for one object across frames.
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
    /// Keys already reported. Reported entries leave the map above so their
    /// slot is reusable -- holding them there let long-lived scenery fill the
    /// table and lock out every object that appeared afterwards, which is what
    /// the FX under investigation always does.
    static TINT_PROGRESS_DONE: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Follow a tinted object over time instead of photographing it once.
///
/// `note_tint_mechanism` already established what the wrong-coloured instances
/// carry: a colour transform, tweened from a darkened base toward blue, and the
/// orange ones sit at the base value. It cannot say *why* they sit there,
/// because it reports each object once. This one films: it counts how often the
/// clip's own timeline advances and how often the composed colour changes, then
/// reports both.
///
/// The two outcomes want opposite fixes, which is the whole point of measuring
/// before writing another one:
///
/// - frames advancing but the colour never changing means the tween does not
///   come from this clip's timeline, so nothing that freezes this clip is
///   responsible and the source of the transform is the thing to find.
/// - frames not advancing means this clip is stuck, and `playing` splits that
///   again: stopped by content versus still playing but not being stepped.
fn note_tint_progression<'gc>(this: DisplayObject<'gc>, context: &RenderContext<'_, 'gc>) {
    // 48 samples is two seconds at 24fps, and a cast FX can be gone before
    // that -- a threshold the subject cannot reach is a threshold that decides
    // what you see, the same way a low cap does. 16 is still long enough for a
    // tween to visibly progress.
    const SAMPLES_BEFORE_REPORT: u32 = 16;
    // Both caps were reached on 2026-08-05 while the wrong-coloured instances
    // were on screen, and a cap that is reached stops reporting for the rest of
    // the session -- so everything after it is invisible. A session's worth of
    // these lines is a couple of MB, which is nothing next to missing the
    // subject for the third run in a row.
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
        // The composed transform above cannot say *where* the colour was
        // written. This is the object's own, and the two split the remaining
        // explanations: differing here means the write is per instance and some
        // are skipped; identical here, with the composed values differing,
        // means the write is on an ancestor and the wrong-coloured instances
        // hang off a different one.
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

/// Report how a tinted object is actually tinted.
///
/// Two instances of one asset drawing in different colours in the same frame
/// narrows to three mechanisms, and they are not distinguishable by looking at
/// the screen: a colour transform that one instance did not receive, a filter
/// that did not apply, or a blend resolving against different content. Each
/// wants a different fix, and three guesses have already been spent here. This
/// says which one is in play by printing what each instance actually carries.
fn note_tint_mechanism<'gc>(this: DisplayObject<'gc>, context: &RenderContext<'_, 'gc>) {
    // Each object reports once, so the cap only exists to bound a pathological
    // scene. The first version set it at 40 and the whole budget was spent on
    // startup objects before the skill under investigation was ever cast --
    // a cap low enough to be reached is a cap that decides what you see. 400
    // was reached too, on 2026-08-05, in a run where the wrong-coloured FX was
    // on screen and went unrecorded.
    const MAX_REPORTS: usize = 4000;

    let url = this.movie().url().to_owned();
    if !flicker_probe_covers(&url) {
        return;
    }

    let blend = this.blend_mode();
    let filters = this.filters();
    let color = context.transform_stack.transform().color_transform;
    let tinted = color != Default::default();
    // Only objects that carry one of the three mechanisms are interesting.
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
    /// Per object: the last state `note_cache_decision` reported, and how many
    /// lines it has spent.
    ///
    /// The first version keyed on (object, camera distance) and reported each
    /// once. That cannot see the thing being chased -- art that is right and
    /// then goes wrong again while the camera holds still -- because the key
    /// does not change when that happens. It also spent its whole budget in 12
    /// seconds, 96% of it on children inside bakes, and a budget that runs out
    /// decides what you get to see.
    static CACHE_DECISION_STATE:
        std::cell::RefCell<std::collections::HashMap<usize, (u64, u32)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A colour quantized coarsely enough that a fade -- which walks a multiplier a
/// little every frame -- reports a few steps instead of a line per frame.
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

/// The names of up to `levels` ancestors, nearest first, for probe output.
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

/// Log every colour written to filtered art, and who received it.
///
/// The cache-decision probe found cape instances whose tinted node carried
/// identity colour through every bake of its life, alongside another instance
/// of the same class, in the same room, that got its tint 0.2s after appearing.
/// Two very different causes fit that -- the tint was written and then lost, or
/// it was never written to that instance -- and the read side cannot tell them
/// apart. A write that never happens leaves nothing to see.
///
/// Note the hole this does not cover: AVM2's `DisplayObject.transform` setter
/// writes through `DisplayObjectBase::set_color_transform` directly rather than
/// the trait method below, so a whole-transform assignment does not appear
/// here. It invalidates the parent's cache on its own, so it is not a
/// correctness gap, but a silent log is not proof on its own.
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

/// Report how an object is drawn -- cache or vectors -- every time that answer,
/// its texture, or its colour changes.
///
/// Item Color Custom comes out right with the camera close and wrong -- the
/// untinted base art -- with it far, and goes wrong again the moment the camera
/// moves after a zoom has put it right. A cache bakes its descendants' colour
/// into the texture while vector rendering composes the tint live, which would
/// explain a colour that depends on how the object is drawn and on nothing the
/// content did.
///
/// What the first run established, on a Necromancer2016JuneDragonCape:
///
/// - the tint is an ordinary `ColorTransform`, on a descendant four levels down
///   (`Symbol168_6`, multiply 0 / add 255,0,0). The note claiming AQW's tint
///   does not arrive through `set_color_transform` was measured on skill FX and
///   does not hold for items.
/// - the cape's first bake ran while every child still carried identity colour,
///   so that texture is untinted art. The tint appeared 1.2s later.
/// - `wants_cache=true` with `has_texture=false` and non-zero `tex` was caught
///   in the same run: `update` ran and the allocation was refused, which is
///   `acceptable_size` and not `should_bypass_offscreen_bitmap_cache` -- the
///   latter returns early, keeping the cache, whenever the bounds meet the
///   view, so its limits only ever apply to art entirely off screen.
///
/// `tex` matters because "cached" alone does not mean a texture was composited:
/// a cache whose allocation was refused holds none and renders as vectors too.
///
/// Reporting on change rather than once per camera distance is what the first
/// run forced. The thing being chased is art that is right and then goes wrong
/// while the camera holds still, and a key that does not move when that happens
/// cannot record it.
fn note_cache_decision<'gc>(
    this: DisplayObject<'gc>,
    context: &RenderContext<'_, 'gc>,
    cache_info: Option<&DrawCacheInfo>,
) {
    // A line per state change is bounded by how much actually changes, so these
    // only stop a pathological oscillator. One object that flips every frame
    // must not be able to silence the rest, hence the per-object share.
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

    // The camera sits at a handful of distances, so bucketing by eighths keeps
    // a slow drift from counting as a new state every frame.
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
        // Read this first. Baking a cache renders the subtree again with
        // `use_bitmap_cache` off for AQW, so every child inside a bake arrives
        // here and reports `vectors` -- without this field, a properly cached
        // item looks like a vector-drawn one from its children's lines alone.
        offscreen = context.is_offscreen,
        // Whether anything asked for a cache at all, which separates "not
        // cached because nobody wanted one" from "wanted one and did not get
        // a texture".
        wants_cache = this.is_bitmap_cached(),
        has_texture,
        tex = texture_size.as_str(),
        // The composed scale is what sizes the cache texture, and it moves with
        // the window as well as the camera; the view scale is logged beside it
        // so a zoom can be told from a resize.
        scale = format!("{scale:.3}"),
        view_scale = format!("{view_scale:.3}"),
        name = this.name().map(|n| n.to_string()).unwrap_or_default(),
        class = class.as_str(),
        depth = this.depth(),
        // Which avatar this belongs to. Instances of the same item class that
        // behave differently are the whole puzzle, and the holder is the first
        // thing that could distinguish them -- the player's own avatar from
        // another player's, or one of AQW's offscreen holders.
        holders = ancestor_names(this, 4).as_str(),
        // Composed against own, the same split the tint probes use: a tint that
        // shows up composed but not own was written above this object, and one
        // that shows in neither was written below -- which is the case a cache
        // bakes and this log cannot see directly.
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
        // Behind its own switch, not the general diagnostics one: these two
        // run a map lookup per display object per frame, which is affordable
        // for a targeted hunt but would tax — and so distort — the memory
        // sweep that shares that flag.
        if aqw_flicker_probe_enabled() {
            note_position_oscillation(this, context);
            note_map_clip_animation(this);
            note_tint_mechanism(this, context);
            note_tint_progression(this, context);
        }
    }

    let blend_mode = this.blend_mode();
    // Captured here rather than at the emit site below, because by then this
    // object's *own* bake has been pushed and popped again. What decides the
    // booking is whether an *ancestor* was baking when this object was reached,
    // which is exactly the stack as it stands on the way in.
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

    // A filtered object has no other way to get its filters applied, which is
    // why `recheck_cache_as_bitmap` forces a cache on to anything carrying one.
    // Inside an AQW bake `use_bitmap_cache` is off (see the offscreen context
    // below), and without this exemption the filter would simply not happen.
    // Kill-switch: `RUFFLE_AQW_NO_NESTED_FILTER_CACHE=1`.
    //
    // `cache_filtered_children` is what limits this to the bake. Reading
    // `use_bitmap_cache` alone was the first version and it also caught the two
    // other places that switch caching off, where the exemption is not wanted
    // and is expensive: the scaling grid turns it off on the *live* context and
    // draws the subtree nine times, each slice under a different matrix, so a
    // filtered descendant of an AQW panel is dirty on every slice -- nine bakes
    // and up to nine texture allocations per frame, each. AQW's windows are
    // scaling-grid panels and its item icons carry a ColorMatrixFilter, so the
    // inventory is a gridful of them.
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
        // A cache we switched on for avatar art, rather than one the content
        // asked for. These belong to objects that move, so they get neither
        // pixel snapping nor deferral -- both are only safe for art that holds
        // still, and here they show up as shaking and as art left behind during
        // a zoom.
        let aqw_auto_cache = !aqw_avatar_cache_disabled()
            && this
                .as_movie_clip()
                .is_some_and(|clip| clip.is_aqw_avatar_asset_root());
        // A non-finite or absurdly-scaled transform can't yield a usable cache (it
        // would be rejected as "incredibly large" further down anyway). Detect it
        // from the matrix up front so we skip the bounds traversal and the cache
        // math, and never feed NaN/inf into them. AQW-only to keep other content's
        // behavior byte-identical.
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
        // Padded cache textures are only safe when the redraw clear is
        // transparent; an opaque background would paint the padding margin.
        let allow_size_padding = allow_aqw_large_cache
            && !aqw_padded_cache_disabled()
            && this.opaque_background().is_none();

        if let Some(cache) = &mut *this.base().bitmap_cache_mut() {
            if bypass_bitmap_cache {
                // Only a clear that threw a texture away counts: clearing an
                // already-empty cache costs nothing and would drown the signal.
                if aqw_diagnostics_enabled() && cache.bitmap.is_some() {
                    AQW_CACHE_BYPASS_CLEARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                // `bypass_cleared` above answers a narrower question than it
                // looks like it does, and the difference matters: a cache that
                // is bypassed on *every* frame never accumulates a texture, so
                // it has none to throw away and is counted nowhere -- which is
                // exactly the steady state a suppressed cache sits in. Reading
                // zero from it was taken as "the bypass never fires" once
                // already. This counts every bypass, and separately the ones
                // where the object was really on screen.
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
                        // Scaling is done by *stage view matrix* only, nothing in-between
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
                        // Counts through admitted redraws, so it measures how
                        // long the contents have been moving. Reset only on a
                        // frame that finds the cache clean, below.
                        cache.dirty_streak = cache.dirty_streak.saturating_add(1);
                        let redraw_pixels = u64::from(actual_width) * u64::from(actual_height);
                        // The large/small split (and the per-frame pixel budget in
                        // `Player::render`) is calibrated in ~1x windowed pixels.
                        // Fullscreen plus supersampling multiplies every cache's
                        // pixel size by the view scale squared, which reclassified
                        // ordinary combat FX as "large" and starved them on the
                        // small large-redraw quota - chronic deferral, detached
                        // stale weapon art and old-scale damage numbers appearing
                        // only in fullscreen. Normalize the thresholds by the view
                        // scale so classification matches the windowed calibration.
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
                        // Small caches used to bypass the budget entirely ("small is
                        // cheap"), but AQW FX storms (fireworks, ultra-boss skill spam)
                        // run hundreds of small filtered clips at once — each admitted
                        // redraw costs filter passes plus offscreen-pool textures, and
                        // the swarm is what melts FPS. They now draw from their own
                        // per-frame quota instead.
                        let mut admitted_aged = false;
                        let grace_admitted = cache.dirty_streak <= aqw_defer_eligible_frames();
                        let mut can_redraw_cache = if !allow_aqw_large_cache || aqw_auto_cache {
                            // Deferral trades freshness for frame time, which
                            // only works when the art is standing still. An
                            // avatar cache that misses its redraw during a zoom
                            // composites at the previous scale and visibly
                            // detaches, so these always redraw.
                            true
                        } else if grace_admitted {
                            // Too recently settled to stand in for itself: the
                            // texture on hand predates whatever just happened to
                            // this object. See `aqw_defer_eligible_frames`.
                            AQW_CACHE_DEFER_GRACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            true
                        } else if is_large {
                            context.try_reserve_dirty_cache_redraw(redraw_pixels)
                        } else {
                            context.try_reserve_small_cache_redraw(redraw_pixels)
                        };
                        // Budget admission is in render order, so the same objects can
                        // lose the race every frame and stay stale indefinitely. Let
                        // long-starved caches through a small reserved quota.
                        if !can_redraw_cache
                            && cache.deferred_frames >= aqw_stale_cache_aged_frames()
                            && context.try_reserve_aged_cache_redraw()
                        {
                            can_redraw_cache = true;
                            admitted_aged = true;
                        }

                        if can_redraw_cache {
                            // Counted where the bake is actually admitted, not
                            // where dirtiness is detected: a refused redraw
                            // costs nothing and must not read as one.
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
                                // Read before `update` overwrites them, so the
                                // log shows both ends of the ping-pong.
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
                            // Read before the move into `DrawCacheInfo`.
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
                                // A zero side is refused by `NonZero` before any
                                // size gate is consulted, so it is a different
                                // failure with a different fix, and the counts
                                // said the size gate cannot be carrying this:
                                // it warns once per cache and warned once all
                                // session against hundreds of refusals here.
                                suppress_reason = Some(if actual_width == 0 || actual_height == 0 {
                                    suppress_detail = Some(format!(
                                        "{width}x{height}->{actual_width}x{actual_height}"
                                    ));
                                    // The raw numbers, once per cache. A zero
                                    // rect and a rect past 65535 are the two
                                    // ends a non-finite value produces when it
                                    // crosses into `Twips`, and both turned up
                                    // in this object group, so print the sides
                                    // and the matrix rather than the size.
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
                            // Prefer an existing cache while the redraw is deferred.
                            // If this is the first draw, normal vector rendering below
                            // keeps the object visible until its cache is admitted.
                            // The stale texture is anchored where its contents were
                            // rendered; the live bounds may have moved/scaled since.
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
                            // A stale texture only stands in convincingly while the
                            // object's bounds still sit near where its contents were
                            // rendered. A fast animation (a weapon swing) moves them
                            // by hundreds of pixels within a couple frames, and the
                            // anchored old art then shows up detached, floating away
                            // from the object. Past the drift tolerance, skip the
                            // stale draw and let vector rendering below carry the
                            // object this frame (briefly without filters, same
                            // degradation as a fresh denied cache).
                            let current_offset = Point::new(
                                bounds.x_min - base_transform.matrix.tx
                                    + Twips::from_pixels_i32(draw_offset.x),
                                bounds.y_min - base_transform.matrix.ty
                                    + Twips::from_pixels_i32(draw_offset.y),
                            );
                            // The tolerance is calibrated in ~1x windowed
                            // pixels, like the large/small thresholds above,
                            // but the offsets it is compared against are in
                            // view-scaled surface pixels — scale it by the
                            // view too, or fullscreen trips the drift guard on the
                            // ambient motion that windowed absorbs and blinks
                            // the object's filters off for a frame.
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
                        // Settled. The next thing to happen to this object gets
                        // the grace window again.
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
                        // Same raw numbers as the zero case, for the same
                        // reason: these are the two ends of one suspicion, and
                        // comparing them needs the sides and the matrix, not
                        // the size that both of them mangle.
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

    // Placed here rather than beside the other probes at the top of the
    // function because this is the first point where the outcome exists: the
    // preference, the size gates and the redraw budget have all had their say.
    if aqw_flicker_probe_enabled() {
        note_cache_decision(this, context, cache_info.as_ref());
    }

    // We can't hold `cache` (which will hold `base`), so this is split up
    if let Some(cache_info) = cache_info {
        // In order to render an object to a texture, we need to draw its entire bounds.
        // Calculate the offset from tx/ty in order to accommodate any drawings that extend the bounds
        // negatively. A stale (deferred) cache instead anchors at the offset recorded
        // when its texture was actually rendered, so the old contents stay attached to
        // the object rather than jumping to bounds they no longer match.
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
                // The outer cache already captures this subtree. Reusing child
                // cacheAsBitmap objects here duplicates textures and can explode
                // memory in crowded AQW rooms. Filtered children are exempt:
                // a filter is applied when a cache texture is drawn and the
                // vector path has no step that applies one, so switching the
                // cache off silently drops them -- and a ColorMatrixFilter is a
                // colour operation, which is how an item can draw in the wrong
                // colour for exactly as long as an ancestor is cached.
                use_bitmap_cache: !is_aqw_movie_url(this.movie().url()),
                // The exemption belongs to this context and to no other. The
                // two other places that clear `use_bitmap_cache` -- the scaling
                // grid and `BitmapData.draw` -- want the plain behaviour, and
                // inferring the exemption from the flag alone gave the scaling
                // grid nine bakes per frame for every filtered descendant.
                cache_filtered_children: true,
                // Not zero, which is what the first attempt at that exemption
                // (2026-08-06) tripped over: `try_reserve_*` refuses on a zero
                // budget, so a nested cache that went dirty could never be
                // admitted and composited its *stale* texture into the parent's
                // -- art from another moment baked into everything. Deferring is
                // never right in here anyway. It trades freshness for frame
                // time across frames, and a bake is a single frame's work whose
                // result is then held; a deferral inside one is not paid back,
                // it is preserved. The allowance is unbounded because what can
                // reach it is bounded already: only filtered children get a
                // cache here at all.
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
        //
        // Snapping a cache to whole pixels is what Flash does, and it is right
        // for a cache the content asked for. The AQW avatar caches are ours,
        // not the content's: an avatar walking at sub-pixel speed would jump
        // half a pixel every frame, which reads as the art shaking. Those draw
        // at their true position instead -- the drift guard only reacts above
        // 16px, so nothing else catches this.
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

        // Growing an *empty* rect is meaningless, and doing it is not harmless:
        // `Rectangle::INVALID` is the sentinel for "no content", `is_valid`
        // recognises it by `x_min` alone, and a filter moves `x_min` off the
        // sentinel. The empty rect then passes as a real one whose far corner
        // sits at the sentinel plus the filter growth, and the first ancestor to
        // union it inherits a bounding box millions of pixels wide.
        //
        // Measured 2026-08-12: an avatar with an empty slot -- `Blank.swf` hair,
        // `unarmed.swf`, an unused pose -- carrying a glow produced
        // `x_min=21851 x_max=134218297`, which is the sentinel 134217727 plus
        // 570 twips of glow. Bounds that size are refused a cache, so the whole
        // subtree renders vector every frame and every blend placement inside it
        // becomes its own pass: one such item was emitting ~2490 complex blends
        // per sweep window and holding the frame at ~68 ms.
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
    /// Sets the color transform of this object.
    /// This invalidates any ancestor's cacheAsBitmap automatically.
    fn set_color_transform(self, color_transform: ColorTransform) {
        // Every other visual property does this -- x, y, rotation, scale and
        // perspective all tell ancestors to regenerate. Colour did not, and a
        // cache bakes its descendants' colour into the texture: the object's
        // own transform is re-applied when the cache is drawn, but a child's
        // is not. Content that tints named child parts after a cached ancestor
        // exists therefore keeps the untinted texture, and whether the tint
        // survives comes down to which happened first. That reads as the same
        // asset drawing in different colours from one instance to the next.
        let changed = self.base().set_color_transform(color_transform);
        if changed && aqw_flicker_probe_enabled() {
            note_colour_write(self, &color_transform);
        }
        if changed && let Some(parent) = self.parent() {
            // Self-transform changes are handled when the cache is drawn, so
            // only ancestors need telling -- matching the sibling setters.
            //
            // Exempting these from the redraw budget was tried and reverted:
            // content animates colour (fades) every frame, so "the colour
            // changed" is true almost always, and the exemption removed the
            // budget rather than making an exception to it. The mark below is
            // not that: the budget still applies, and a cache that loses the
            // race still waits. What it stops is the *stale texture* standing
            // in while it waits, which for colour is a visible flash in the old
            // colour. Such a cache renders as vectors until it is admitted --
            // the same escape the drift guard already takes for position.
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

    /// Whether this object belongs to an AQW player-asset Loader that was
    /// removed from the display list. These detached trees may remain strongly
    /// referenced by game code, but should not consume frame time until they
    /// are attached again.
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

    /// Live variant of `is_in_detached_aqw_avatar_loader` that doesn't wait
    /// for the one-frame grace period to confirm the detach is persistent.
    /// See `LoaderDisplay::is_currently_parentless_aqw_avatar_loader`.
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

        // The new ancestor chain has never seen this object, and the old one
        // just lost a child; both have frame work to find. Marking after the
        // reparent walks the chain the object actually hangs from now.
        // A removal reaches here before `on_parent_removed` has put the object
        // on the orphan list, so this mark finds nothing to note. That is fine:
        // `add_orphan_obj` makes the entry pending itself, which is the same
        // thing the mark would have said.
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

    /// This method is called when an object without a parent is attached.
    /// It may be overwritten to restore implementation-specific state.
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

    /// Drop any rendered bitmap cache while preserving the object's cache preference.
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

    /// Record that a frame pass has work to find here, and make sure it can be
    /// reached: a nested goto skips clean subtrees, so an ancestor that still
    /// looks clean would hide this object from the walk.
    ///
    /// Stops at the first ancestor already marked, which is what keeps this
    /// cheap on the hot mutation paths -- in a settled tree the parent is
    /// almost always marked already.
    /// Whether a frame pass may skip this subtree entirely.
    ///
    /// Only inside a nested goto. The ordinary frame always walks everything,
    /// so a mark this scheme fails to set costs one frame of latency there
    /// rather than leaving an object unconstructed forever.
    #[no_dynamic]
    fn can_skip_frame_pass(self, context: &UpdateContext<'gc>) -> bool {
        *context.aqw_nested_goto && !self.base().subtree_needs_frame() && !frame_skip_disabled()
    }

    /// The subtree mark on its own, without the nested-goto gate above.
    ///
    /// Probe-only, and it must stay that way. **Honouring this mark in the
    /// ordinary tick's orphan walk was built and disproved on 2026-08-07: it
    /// fails `avm2/orphan_movie_complex`, `avm2/orphan_movie_reorder`,
    /// `avm2/orphan_removeobject`, `avm2/avm2_catchup_dobj` and
    /// `timeline/missing_frame_scripts`.** A periodic full walk to bound the
    /// damage (every 24 ticks, staggered by address) did not save it either.
    ///
    /// The reason is structural rather than a missing mark somewhere. This flag
    /// tracks *construction* -- `needs_frame_construction` is literally "does
    /// `construct_frame` still have something to do" -- while the tick's orphan
    /// walk runs three passes, and `enter_frame` has to advance every playing
    /// timeline in the subtree. Nothing sets the mark for that, and nothing
    /// should: inside a nested goto the mark is a verdict precisely because the
    /// ordinary tick is the backstop that catches what it misses.
    ///
    /// A sound skip needs a *different* signal -- one that says no timeline
    /// under here will advance, which means propagating a playing-descendant
    /// count the way this mark propagates. That is new machinery, not a wider
    /// read of this one. What it would be worth is measured: see the
    /// `orph_clean_*` columns and `AQW_ORPHAN_POP`.
    #[no_dynamic]
    fn aqw_subtree_clean(self) -> bool {
        !self.base().subtree_needs_frame()
    }

    /// Recompute this object's mark from its own pending work and its children's
    /// marks, after a pass has walked it. Called by the frame-script pass, which
    /// is the last one over the tree -- clearing in the construct pass would hide
    /// the work from the pass that still has to run.
    /// Whether `construct_frame` still has something to do for this object, so
    /// that the subtree mark survives a pass that could not finish.
    ///
    /// Paired with `construct_frame`: the default is `false` because the default
    /// `construct_frame` is a no-op, and a type that overrides one must override
    /// the other. Answering `object2().is_none()` for everything is the trap --
    /// types that never allocate an AVM2 object would pin the mark forever and
    /// propagate it up, which measured out at ~60% of orphan visits.
    fn needs_frame_construction(self) -> bool {
        false
    }

    #[no_dynamic]
    fn settle_subtree_needs_frame(self, children_need: bool) {
        // Deliberately does *not* keep the mark alive for an object that still
        // has no AVM2 side. That looks like the safe thing and is the opposite:
        // several types never allocate an `object2` at all (an empty
        // `construct_frame`, or any object in an AVM1 movie), so the mark would
        // never clear and would propagate up through `children_need` forever.
        // Measured 2026-08-01: it pinned ~60% of orphan visits permanently
        // dirty, defeating the skip.
        //
        // Nothing is lost, because the skip only applies inside a nested goto
        // and the ordinary frame always walks the whole tree -- an object that
        // does need constructing gets constructed there, within one frame.
        self.base()
            .set_subtree_needs_frame(children_need || self.needs_frame_construction());
    }

    /// Mark this object's whole subtree, as well as the path to it.
    ///
    /// A goto re-runs the frame scripts of everything under the clip it acts
    /// on, and those scripts are *discovered* by `construct_frame`
    /// (`check_has_pending_script`) rather than being known in advance. Marking
    /// only upwards would let the construct pass skip a descendant, so its
    /// script would never be found and never run -- which is exactly what
    /// `timeline/frame_script_cleanup_goto2` catches.
    ///
    /// Costs one walk of the gotoed clip's subtree, which the frame would have
    /// walked anyway. The stage's other ~360k objects stay skippable.
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
        // Always mark self first. A freshly created object is already marked
        // while its brand new ancestors are not, so an early return on "self is
        // marked" would leave the path to it clean and unreachable.
        self.base().set_subtree_needs_frame(true);

        let mut top: DisplayObject<'gc> = self;
        let mut node = self.parent();
        while let Some(current) = node {
            top = current;
            if current.base().subtree_needs_frame() {
                // Already reachable, so whatever this walk would have told the
                // orphan loops was told when that ancestor was marked.
                return;
            }
            current.base().set_subtree_needs_frame(true);
            node = current.parent();
        }

        // The walk ended at a parentless object. If that is not the stage, this
        // subtree hangs off the orphan list, and the next nested goto owes that
        // root a pass.
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
        // Read before the clear below, which is the only place it survives to.
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

        /// Whether this object, or anything below it, may have work for
        /// `construct_frame` or `run_frame_scripts` to find.
        ///
        /// An explicit AVM2 goto runs a whole recursive frame, which walks the
        /// entire stage. Content that gotos in an `enterFrame` handler pays that
        /// walk once per goto: measured 2026-08-01 in AQW at ~2270 nested frames
        /// per 48 rendered ones over a tree of ~360k objects, which was 86-89%
        /// of the frame. Almost none of that tree can have changed between two
        /// gotos in the same frame, so a subtree that is known clean is skipped.
        ///
        /// Set on creation and by every mutation a frame pass reacts to;
        /// recomputed bottom-up as the passes walk. Only *consulted* inside a
        /// nested goto -- the ordinary frame still walks everything, so a missed
        /// mark costs at most a frame of latency instead of breaking the object.
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

    /// Set a variable, read the flag, restore. Serialised by the mutex below,
    /// because the environment is process-wide and tests run in parallel.
    fn with_var(value: Option<&str>, default: bool) -> bool {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "RUFFLE_AQW_ENV_FLAG_TEST";
        match value {
            // SAFETY: no other thread touches this variable; the lock above
            // keeps these tests from racing each other.
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

    /// The regression this parser exists for: presence used to be the whole
    /// test, so `NO_SOMETHING=0` switched the thing it names OFF, which is the
    /// opposite of what it reads as.
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
        // The path in front of the segment is free to move.
        assert!(url_path_has_segment(
            "https://cdn.example.com/GameFiles/items/Sword.swf",
            "gamefiles"
        ));
        assert!(url_path_has_segment(
            "https://game.aq.com/game/gamefiles/items/Sword.swf?v=2",
            "items"
        ));
        // A partial name is not a segment, and neither is a query value.
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
