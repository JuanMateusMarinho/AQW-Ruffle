use crate::descriptors::Descriptors;
use crate::globals::Globals;
use fnv::FnvHashMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

type PoolInner<T> = Mutex<Vec<T>>;
type Constructor<Type, Description> = Box<dyn Fn(&Descriptors, &Description) -> Type>;

/// Maintenance ticks (≈ frames) an idle pooled texture may sit unused before
/// the maintenance pass frees it regardless of the retention budget. Long
/// enough to survive deferred cache redraws that skip a texture for a couple
/// of seconds, short enough to drain the piles left behind by map changes.
const POOL_IDLE_EVICT_TICKS: u64 = 120;

/// Entries idle for fewer ticks than this are never evicted, even when the
/// pool is over budget. This protects the recent working set: evicting a
/// texture that gets reallocated at the same size next frame frees nothing —
/// it just cycles allocations through the driver's deferred-destruction
/// queue every frame, which is exactly the commit-memory creep the pool is
/// supposed to prevent.
const POOL_PROTECT_TICKS: u64 = 4;

/// How long hard VRAM pressure must persist before the escape reset fires.
/// Short paging episodes (map-entry bursts) resolve themselves; a streak
/// this long means the process is parked over its OS budget.
const POOL_HARD_RESET_AFTER_TICKS: u64 = 96;

/// Minimum spacing between escape resets. The reset costs one visible hitch
/// (a whole pool's worth of deferred destruction plus a warmup of fresh
/// allocations), so if even that can't get the process back under budget,
/// repeating it faster just turns the hitch periodic.
const POOL_HARD_RESET_COOLDOWN_TICKS: u64 = 600;

/// How long pressure must stay released before the escape reset re-arms.
/// The reset drains the pool, and pool retention is exactly what the valve
/// measures, so the reading always dips right after a reset — even a futile
/// one that live demand is about to undo. Requiring a sustained release keeps
/// a one-shot escape from becoming a periodic drain.
const POOL_EPISODE_END_TICKS: u64 = 240;

/// Round an offscreen render-target dimension up to a coarse bucket (powers
/// of two up to 1024, multiples of 512 above). Animated filtered content
/// changes bounds by a few pixels every frame; exact-size pool keys make
/// every step a brand-new texture allocation, and that allocate/destroy
/// churn is what inflates driver memory in busy rooms. Callers that opt in
/// must treat the returned texture as larger than the content: render with
/// a viewport confined to the logical size and sample only the logical UV
/// sub-rectangle.
pub fn quantize_pool_dimension(dim: u32) -> u32 {
    if dim <= 16 {
        16
    } else if dim <= 1024 {
        dim.next_power_of_two()
    } else {
        dim.next_multiple_of(512)
    }
}

#[derive(Debug, Default)]
pub struct TexturePool {
    pools: FnvHashMap<TextureKey, BufferPool<(wgpu::Texture, wgpu::TextureView), AlwaysCompatible>>,
    globals_cache: FnvHashMap<GlobalsKey, Arc<Globals>>,
    /// Advanced once per frame by `maintain`; pooled entries are stamped with
    /// it when returned, giving each an idle age.
    clock: Arc<AtomicU64>,
    /// Cumulative number of textures actually created (pool misses).
    total_allocs: Arc<AtomicU64>,
    /// Cumulative number of idle textures freed by maintenance.
    total_frees: u64,
    /// Consecutive maintenance ticks spent under hard VRAM pressure; drives
    /// the escape reset.
    hard_pressure_streak: u64,
    /// Clock value before which another escape reset may not fire.
    hard_reset_cooldown_until: u64,
    /// Whether the escape reset already fired during the current hard-
    /// pressure episode. When the reset works (arena blocks returned, VRAM
    /// back under budget) pressure releases and re-arms it; when the live
    /// demand instantly refills the pool it does NOT re-fire — a futile
    /// reset repeated on cooldown is just a periodic 2 GB hitch.
    ///
    /// Re-arming requires a *sustained* release (`POOL_EPISODE_END_TICKS`),
    /// not an instantaneous one: the reset drains the pool, and the valve now
    /// measures pool retention, so the tick right after a reset always looks
    /// like a success.
    hard_reset_fired_this_episode: bool,
    /// Consecutive maintenance ticks spent below hard pressure; gates
    /// re-arming `hard_reset_fired_this_episode`.
    low_pressure_streak: u64,
}

impl TexturePool {
    pub fn new() -> Self {
        Default::default()
    }

    fn bytes_per_texture(key: &TextureKey) -> u64 {
        u64::from(key.size.width)
            * u64::from(key.size.height)
            * u64::from(key.size.depth_or_array_layers)
            * u64::from(key.sample_count)
            * u64::from(key.format.block_copy_size(None).unwrap_or(4))
    }

    /// Bytes of GPU texture memory *currently held* by this pool (idle textures
    /// available for reuse), computed from the live buckets.
    pub fn retained_bytes(&self) -> u64 {
        self.pools
            .iter()
            .map(|(key, pool)| pool.available_len() as u64 * Self::bytes_per_texture(key))
            .sum()
    }

    /// `(cumulative allocations, cumulative frees, retained bytes)` — the
    /// allocation delta per second is the churn measurement the diagnostics
    /// sweep reports.
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.total_allocs.load(Ordering::Relaxed),
            self.total_frees,
            self.retained_bytes(),
        )
    }

    /// Once-per-frame pool maintenance. Advances the idle clock, frees
    /// long-idle textures, and if retention still exceeds `budget`, frees the
    /// longest-idle entries beyond it — but never ones used within the
    /// protection window. A hot working set larger than the budget is
    /// deliberately left alone when VRAM is healthy: freeing it would only
    /// re-allocate the same sizes next frame, feeding the driver's
    /// deferred-destruction backlog without reclaiming anything. Frees are
    /// capped at `max_free_bytes` per call so piled-up destruction spreads
    /// across frames instead of stalling one (the periodic hitch).
    ///
    /// `vram_pressure` (0 = healthy, 1 = soft, 2 = hard, from the process
    /// GPU-memory budget) picks the eviction stance. Every gradual policy
    /// under pressure was field-tested on 13/07 and failed:
    /// - Fast draining (budget 0-64 MB, ≥128 MB/frame): multi-GB destroy/
    ///   create bang-bang — the redraw valve's deferrals make alive targets
    ///   look idle, and the drain teleports the once-a-second VRAM reading
    ///   through the release threshold.
    /// - Holding everything: the process parks over its OS budget and WDDM
    ///   pages live textures (15 fps).
    /// - Slow bleeding (24 MB/frame toward a 512 MB floor): a drain↔realloc
    ///   loop against the deferred working set — allocating while paging is
    ///   synchronously expensive, 5 fps.
    /// What demonstrably recovers (observed when the window was minimized:
    /// VRAM 7.6→4.2 GB, then steady 24 fps in the same room) is a FULL
    /// one-shot drain: emptying the pool zeroes whole driver-arena blocks,
    /// which is what actually returns memory to the OS — a partial bleed
    /// leaves every block fragmented and returns nothing. So under pressure
    /// the pool only tightens its long-idle pass, and if hard pressure
    /// *persists* it fires one full reset (one hitch), then cools down.
    pub fn maintain(&mut self, budget: u64, max_free_bytes: u64, vram_pressure: u8) {
        let now = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        let idle_ticks = match vram_pressure {
            0 => POOL_IDLE_EVICT_TICKS,
            1 => 48,
            _ => 32,
        };

        // Escape reset: sustained hard pressure means the process is parked
        // over its OS budget and paging; nothing gradual gets it back under.
        // At most once per pressure episode — see `hard_reset_fired_this_episode`.
        if vram_pressure == 2 {
            self.hard_pressure_streak += 1;
            self.low_pressure_streak = 0;
            if !self.hard_reset_fired_this_episode
                && self.hard_pressure_streak >= POOL_HARD_RESET_AFTER_TICKS
                && now >= self.hard_reset_cooldown_until
            {
                for pool in self.pools.values() {
                    self.total_frees += pool.evict_idle(now, 1, usize::MAX) as u64;
                }
                self.hard_reset_cooldown_until = now + POOL_HARD_RESET_COOLDOWN_TICKS;
                self.hard_pressure_streak = 0;
                self.hard_reset_fired_this_episode = true;
                return;
            }
        } else {
            self.hard_pressure_streak = 0;
            // Re-arm only after pressure has STAYED released. Draining the pool
            // is what the reset does, and pool retention is what the valve
            // reads, so the instant after a reset the signal always says
            // "recovered" — even when the reset was futile and live demand is
            // about to refill it. Re-arming on that reading turns the one-shot
            // escape into a periodic drain, and recycling a drained target that
            // a deferred cache still references paints one object with
            // another's art.
            self.low_pressure_streak += 1;
            if self.low_pressure_streak >= POOL_EPISODE_END_TICKS {
                self.hard_reset_fired_this_episode = false;
            }
        }

        let mut free_left = max_free_bytes;

        // Pass 1: long-idle entries go regardless of budget.
        for (key, pool) in self.pools.iter() {
            if free_left == 0 {
                break;
            }
            let per = Self::bytes_per_texture(key).max(1);
            let max_items = (free_left / per) as usize;
            if max_items == 0 {
                continue;
            }
            let dropped = pool.evict_idle(now, idle_ticks, max_items) as u64;
            free_left = free_left.saturating_sub(dropped * per);
            self.total_frees += dropped;
        }

        // Pass 2 (healthy VRAM only): keep the idle hoard under the normal
        // retention budget, sparing the recent working set. Under pressure
        // this is deliberately off — gradual budget eviction only churns
        // against the deferred working set (see above).
        if vram_pressure > 0 {
            return;
        }
        let retained = self.retained_bytes();
        if retained <= budget || free_left == 0 {
            return;
        }
        let mut over = (retained - budget).min(free_left);
        for (key, pool) in self.pools.iter() {
            if over == 0 {
                break;
            }
            let per = Self::bytes_per_texture(key).max(1);
            let want = (over / per).max(1) as usize;
            let dropped = pool.evict_idle(now, POOL_PROTECT_TICKS, want) as u64;
            over = over.saturating_sub(dropped * per);
            self.total_frees += dropped;
        }
    }

    pub fn get_texture(
        &mut self,
        descriptors: &Descriptors,
        size: wgpu::Extent3d,
        usage: wgpu::TextureUsages,
        format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> PoolEntry<(wgpu::Texture, wgpu::TextureView), AlwaysCompatible> {
        let key = TextureKey {
            size,
            usage,
            format,
            sample_count,
        };
        let clock = self.clock.clone();
        let allocs = self.total_allocs.clone();
        let pool = self.pools.entry(key).or_insert_with(|| {
            let label = if cfg!(feature = "render_debug_labels") {
                use std::sync::atomic::AtomicU32;
                static ID_COUNT: AtomicU32 = AtomicU32::new(0);
                let id = ID_COUNT.fetch_add(1, Ordering::Relaxed);
                create_debug_label!("Pooled texture {}", id)
            } else {
                None
            };
            BufferPool::new_with_clock(
                Box::new(move |descriptors, _description| {
                    allocs.fetch_add(1, Ordering::Relaxed);
                    let texture = descriptors.device.create_texture(&wgpu::TextureDescriptor {
                        label: label.as_deref(),
                        size,
                        mip_level_count: 1,
                        sample_count,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        view_formats: &[format],
                        usage,
                    });
                    let view = texture.create_view(&Default::default());
                    (texture, view)
                }),
                clock,
            )
        });
        pool.take(descriptors, AlwaysCompatible)
    }

    pub fn get_globals(
        &mut self,
        descriptors: &Descriptors,
        viewport_width: u32,
        viewport_height: u32,
    ) -> Arc<Globals> {
        self.globals_cache
            .entry(GlobalsKey {
                viewport_width,
                viewport_height,
            })
            .or_insert_with(|| {
                Arc::new(Globals::new(
                    &descriptors.device,
                    &descriptors.bind_layouts.globals,
                    viewport_width,
                    viewport_height,
                ))
            })
            .clone()
    }
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
struct TextureKey {
    size: wgpu::Extent3d,
    usage: wgpu::TextureUsages,
    format: wgpu::TextureFormat,
    sample_count: u32,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
struct GlobalsKey {
    viewport_width: u32,
    viewport_height: u32,
}

pub trait BufferDescription: Clone + Debug {
    type Cost: Ord;

    /// If the potential buffer represented by this description (`self`)
    /// fits another existing buffer and its description (`other`),
    /// return the cost to use that buffer instead of making a new one.
    ///
    /// Cost is an arbitrary unit, but lower is better.
    /// None means that the other buffer cannot be used in place of this one.
    fn cost_to_use(&self, other: &Self) -> Option<Self::Cost>;
}

#[derive(Clone, Debug)]
pub struct AlwaysCompatible;

impl BufferDescription for AlwaysCompatible {
    type Cost = ();

    fn cost_to_use(&self, _other: &Self) -> Option<()> {
        Some(())
    }
}

pub struct BufferPool<Type, Description: BufferDescription> {
    /// Idle items, each stamped with the pool clock value at return time.
    available: Arc<PoolInner<(Type, Description, u64)>>,
    constructor: Constructor<Type, Description>,
    /// Shared with `PoolEntry`s so returns are stamped with the owner's
    /// frame clock. Pools whose owner never advances it (`new`) simply see
    /// every entry as age 0.
    clock: Arc<AtomicU64>,
}

impl<Type, Description: BufferDescription> Debug for BufferPool<Type, Description> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool").finish()
    }
}

impl<Type, Description: BufferDescription> BufferPool<Type, Description> {
    pub fn new(constructor: Constructor<Type, Description>) -> Self {
        Self::new_with_clock(constructor, Arc::new(AtomicU64::new(0)))
    }

    pub fn new_with_clock(
        constructor: Constructor<Type, Description>,
        clock: Arc<AtomicU64>,
    ) -> Self {
        Self {
            available: Arc::new(Mutex::new(vec![])),
            constructor,
            clock,
        }
    }

    /// Number of idle items currently available for reuse.
    pub fn available_len(&self) -> usize {
        self.available
            .lock()
            .expect("Should not be able to lock recursively")
            .len()
    }

    /// Drop up to `max_items` idle items whose age (ticks since return, per
    /// the shared clock) is at least `min_age`, oldest first. Returns how
    /// many were dropped. Items still checked out are unaffected.
    pub fn evict_idle(&self, now: u64, min_age: u64, max_items: usize) -> usize {
        let mut guard = self
            .available
            .lock()
            .expect("Should not be able to lock recursively");
        let mut candidates: Vec<usize> = (0..guard.len())
            .filter(|&i| now.saturating_sub(guard[i].2) >= min_age)
            .collect();
        candidates.sort_by_key(|&i| guard[i].2);
        candidates.truncate(max_items);
        // Remove back-to-front so earlier indices stay valid.
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let dropped = candidates.len();
        for i in candidates {
            guard.swap_remove(i);
        }
        dropped
    }

    pub fn take(
        &self,
        descriptors: &Descriptors,
        description: Description,
    ) -> PoolEntry<Type, Description> {
        let mut guard = self
            .available
            .lock()
            .expect("Should not be able to lock recursively");
        let mut best: Option<(Description::Cost, usize)> = None;
        for i in 0..guard.len() {
            if let Some(cost) = description.cost_to_use(&guard[i].1) {
                if let Some(best) = &mut best {
                    if best.0 > cost {
                        *best = (cost, i);
                    }
                } else if best.is_none() {
                    best = Some((cost, i));
                }
            }
        }

        let (item, used_description) = if let Some((_, best)) = best {
            let (item, description, _stamp) = guard.swap_remove(best);
            (item, description)
        } else {
            let item = (self.constructor)(descriptors, &description);
            (item, description)
        };
        PoolEntry {
            item: Some(item),
            description: used_description,
            pool: Arc::downgrade(&self.available),
            clock: self.clock.clone(),
        }
    }
}

pub struct PoolEntry<Type, Description: BufferDescription> {
    item: Option<Type>,
    description: Description,
    pool: Weak<PoolInner<(Type, Description, u64)>>,
    clock: Arc<AtomicU64>,
}

impl<Type, Description: BufferDescription> Debug for PoolEntry<Type, Description>
where
    Type: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PoolEntry").field(&self.item).finish()
    }
}

impl<Type, Description: BufferDescription> Drop for PoolEntry<Type, Description> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take()
            && let Some(pool) = self.pool.upgrade()
        {
            let stamp = self.clock.load(Ordering::Relaxed);
            pool.lock()
                .expect("Should not be able to lock recursively")
                .push((item, self.description.clone(), stamp))
        }
    }
}

impl<Type, Description: BufferDescription> Deref for PoolEntry<Type, Description> {
    type Target = Type;

    fn deref(&self) -> &Self::Target {
        self.item.as_ref().expect("Item should exist until dropped")
    }
}
