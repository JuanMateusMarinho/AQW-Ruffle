//! Live Rust allocation accounting.
//!
//! The sweep already reports what the process holds in total (`commit_mb`) and
//! what the renderer holds in textures and pools. Everything left over is
//! either Rust allocations or the graphics driver's own bookkeeping — and
//! those two point at opposite fixes, one in this codebase and one outside it.
//! Nothing already measured can tell them apart, because the remainder is
//! defined by subtraction; only a global allocator can attribute it directly.
//!
//! NOTE: installing [`CountingAllocator`] taxes every allocation in the
//! process with two atomics, so it is deliberately NOT installed. Because it
//! is not, [`heap_bytes`] reads zero, and the sweep's `heap_mb` column was
//! dropped rather than keep reporting a constant zero. To take the measurement
//! again, add to `desktop/src/main.rs`:
//!
//! ```ignore
//! #[global_allocator]
//! static GLOBAL_ALLOCATOR: ruffle_core::heap_stats::CountingAllocator =
//!     ruffle_core::heap_stats::CountingAllocator;
//! ```
//!
//! and read [`heap_bytes`] from wherever the number is wanted — then take the
//! allocator back out before building the release.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};

static ALLOCATED: AtomicI64 = AtomicI64::new(0);

/// Bytes currently held by Rust allocations, when [`CountingAllocator`] is
/// installed as the global allocator. Reads zero if it is not.
pub fn heap_bytes() -> i64 {
    ALLOCATED.load(Ordering::Relaxed)
}

/// A pass-through allocator that keeps a running total of live bytes.
pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        ALLOCATED.fetch_sub(layout.size() as i64, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            // Only the delta: the old block is gone and the new one is live.
            ALLOCATED.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        }
        new_ptr
    }
}
