use crate::descriptors::Descriptors;
use crate::globals::Globals;
use fnv::FnvHashMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

type PoolInner<T> = Mutex<Vec<T>>;
type Constructor<Type, Description> = Box<dyn Fn(&Descriptors, &Description) -> Type>;

pub(crate) const POOL_IDLE_EVICT_TICKS: u64 = 120;

const POOL_PROTECT_TICKS: u64 = 4;

const POOL_HARD_RESET_AFTER_TICKS: u64 = 96;

const POOL_HARD_RESET_COOLDOWN_TICKS: u64 = 600;

const POOL_EPISODE_END_TICKS: u64 = 240;

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
    clock: Arc<AtomicU64>,
    total_allocs: Arc<AtomicU64>,
    total_frees: u64,
    hard_pressure_streak: u64,
    hard_reset_cooldown_until: u64,
    hard_reset_fired_this_episode: bool,
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

    pub fn retained_bytes(&self) -> u64 {
        self.pools
            .iter()
            .map(|(key, pool)| pool.available_len() as u64 * Self::bytes_per_texture(key))
            .sum()
    }

    pub fn largest_buckets(&self, limit: usize) -> Vec<(u32, u32, usize, u64)> {
        let mut buckets: Vec<(u32, u32, usize, u64)> = self
            .pools
            .iter()
            .filter_map(|(key, pool)| {
                let count = pool.available_len();
                if count == 0 {
                    return None;
                }
                Some((
                    key.size.width,
                    key.size.height,
                    count,
                    count as u64 * Self::bytes_per_texture(key),
                ))
            })
            .collect();
        buckets.sort_unstable_by_key(|&(.., bytes)| std::cmp::Reverse(bytes));
        buckets.truncate(limit);
        buckets
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.total_allocs.load(Ordering::Relaxed),
            self.total_frees,
            self.retained_bytes(),
        )
    }

    pub fn maintain(
        &mut self,
        budget: u64,
        max_free_bytes: u64,
        vram_pressure: u8,
        healthy_idle_ticks: u64,
    ) {
        let now = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        let idle_ticks = match vram_pressure {
            0 => healthy_idle_ticks,
            1 => 48,
            _ => 32,
        };

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
            self.low_pressure_streak += 1;
            if self.low_pressure_streak >= POOL_EPISODE_END_TICKS {
                self.hard_reset_fired_this_episode = false;
            }
        }

        let mut free_left = max_free_bytes;

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
    available: Arc<PoolInner<(Type, Description, u64)>>,
    constructor: Constructor<Type, Description>,
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

    pub fn available_len(&self) -> usize {
        self.available
            .lock()
            .expect("Should not be able to lock recursively")
            .len()
    }

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
