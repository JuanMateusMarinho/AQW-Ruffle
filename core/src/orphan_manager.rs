//! Special handling for AVM2 orphan objects

use crate::context::UpdateContext;
use crate::display_object::{DisplayObject, DisplayObjectPtr, DisplayObjectWeak, TDisplayObject};
use gc_arena::{Collect, Mutation};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The list of 'orphan' objects - these objects have no parent,
/// so we need to manually run their frames in `run_all_phases_avm2` to match
/// Flash's behavior. Clips are added to this list with `add_orphan_movie`.
/// and are removed automatically by `cleanup_dead_orphans`.
///
/// We store `DisplayObjectWeak`, since we don't want to keep these objects
/// alive if they would otherwise be garbage-collected. The movie will
/// stop ticking whenever garbage collection runs if there are no more
/// strong references around (this matches Flash's behavior).
#[derive(Collect)]
#[collect(no_drop)]
pub struct OrphanManager<'gc> {
    orphans: Rc<Vec<DisplayObjectWeak<'gc>>>,

    pending: Vec<DisplayObjectWeak<'gc>>,

    #[collect(require_static)]
    listed: HashMap<usize, u64>,

    #[collect(require_static)]
    next_orphan_order: u64,
}

impl<'gc> OrphanManager<'gc> {
    fn orphans_mut(&mut self) -> &mut Vec<DisplayObjectWeak<'gc>> {
        Rc::make_mut(&mut self.orphans)
    }

    pub fn is_still_orphan(dobj: DisplayObject<'gc>) -> bool {
        dobj.parent().is_none() && !dobj.is_in_detached_aqw_avatar_loader()
    }

    pub fn note_orphan_root_dirty(&mut self, dobj: DisplayObject<'gc>) {
        if !self.listed.contains_key(&(dobj.as_ptr() as usize)) {
            return;
        }

        if crate::display_object::aqw_diagnostics_enabled() {
            crate::display_object::AQW_MARK_BUMPS
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        self.pending.push(dobj.downgrade());
    }

    pub fn take_pending(&mut self, mc: &Mutation<'gc>) -> Vec<DisplayObject<'gc>> {
        if self.pending.is_empty() {
            return Vec::new();
        }

        let mut seen = HashSet::with_capacity(self.pending.len());
        let mut out = Vec::with_capacity(self.pending.len());

        for entry in std::mem::take(&mut self.pending) {
            let ptr = entry.as_ptr() as usize;
            let Some(order) = self.listed.get(&ptr).copied() else {
                continue;
            };
            if !seen.insert(ptr) {
                continue;
            }
            if let Some(dobj) = valid_orphan(entry, mc) {
                out.push((order, dobj));
            }
        }

        out.sort_unstable_by_key(|(order, _)| *order);
        out.into_iter().map(|(_, dobj)| dobj).collect()
    }

    pub fn all_orphans(&self, mc: &Mutation<'gc>) -> Vec<DisplayObject<'gc>> {
        self.orphans
            .iter()
            .filter_map(|entry| valid_orphan(*entry, mc))
            .collect()
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Adds a `MovieClip` to the orphan list. In AVM2, movies advance their
    /// frames even when they are not on a display list. Unfortunately,
    /// multiple SWFS rely on this behavior, so we need to match Flash's
    /// behavior. This should not be called manually - `movie_clip` will
    /// call it when necessary.
    pub fn add_orphan_obj(&mut self, dobj: DisplayObject<'gc>) {
        // Note: comparing pointers is correct because GcWeak keeps its allocation alive,
        // so the pointers can't overlap by accident.
        if self
            .orphans
            .iter()
            .all(|d| !std::ptr::eq(d.as_ptr(), dobj.as_ptr()))
        {
            self.orphans_mut().push(dobj.downgrade());
            let order = self.next_orphan_order;
            self.next_orphan_order = self.next_orphan_order.saturating_add(1);
            self.listed.insert(dobj.as_ptr() as usize, order);
            if crate::display_object::aqw_diagnostics_enabled() {
                crate::display_object::AQW_ORPHAN_ADDS
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.pending.push(dobj.downgrade());
        }
    }

    pub fn remove_orphan_obj(&mut self, dobj: DisplayObject<'gc>) {
        self.orphans_mut()
            .retain(|orphan| !std::ptr::eq(orphan.as_ptr(), dobj.as_ptr()));
        self.listed.remove(&(dobj.as_ptr() as usize));
    }

    pub fn remove_orphan_objs(&mut self, objects: &[DisplayObject<'gc>]) {
        let object_ptrs: HashSet<*const DisplayObjectPtr> =
            objects.iter().map(|object| object.as_ptr()).collect();
        self.orphans_mut()
            .retain(|orphan| !object_ptrs.contains(&orphan.as_ptr()));
        for object in objects {
            self.listed.remove(&(object.as_ptr() as usize));
        }
    }

    pub fn len(&self) -> usize {
        self.orphans.len()
    }

    pub fn each_orphan_obj(
        context: &mut UpdateContext<'gc>,
        mut f: impl FnMut(DisplayObject<'gc>, &mut UpdateContext<'gc>),
    ) {
        // Clone the Rc before iterating over it. Any modifications must go through
        // `Rc::make_mut` in `orphan_objects_mut`, which will leave this `Rc` unmodified.
        // This ensures that any orphan additions/removals done by `f` will not affect
        // the iteration in this method.
        let orphan_objs: Rc<_> = context.orphan_manager.orphans.clone();

        for orphan in orphan_objs.iter() {
            if let Some(dobj) = valid_orphan(*orphan, context.gc()) {
                f(dobj, context);
            }
        }
    }

    /// Called at the end of `run_all_phases_avm2` - removes any movies
    /// that have been garbage collected, or are no longer orphans
    /// (they've since acquired a parent).
    pub fn cleanup_dead_orphans(&mut self, mc: &Mutation<'gc>) {
        let mut dropped: Vec<usize> = Vec::new();

        self.orphans_mut().retain(|d| {
            let keep = if let Some(dobj) = valid_orphan(*d, mc) {
                // All clips that become orphaned (have their parent removed, or start out with no parent)
                // get added to the orphan list. However, there's a distinction between clips
                // that are removed from a RemoveObject tag, and clips that are removed from ActionScript.
                //
                // Clips removed from a RemoveObject tag only stay on the orphan list until the end
                // of the frame - this lets them run a framescript (with 'this.parent == null')
                // before they're removed. After that, they're removed from the orphan list,
                // and will not be run in any way.
                //
                // Clips removed from ActionScript stay on the orphan list, and will be run
                // indefinitely (if there are no remaining strong references, they will eventually
                // be garbage collected).
                //
                // To detect this, we check 'placed_by_avm2_script'. This flag get set to 'true'
                // for objects constructed from ActionScript, and for objects moved around
                // in the timeline (add/remove child, swap depths) by ActionScript. A
                // RemoveObject tag will only affect objects instantiated by the timeline,
                // which have not been moved in the displaylist by ActionScript. Therefore,
                // any orphan we see that has 'placed_by_avm2_script()' should stay on the orphan
                // list, because it was not removed by a RemoveObject tag.
                dobj.placed_by_avm2_script()
            } else {
                false
            };

            if !keep {
                dropped.push(d.as_ptr() as usize);
            }
            keep
        });

        for ptr in dropped {
            self.listed.remove(&ptr);
        }
    }
}

impl<'gc> Default for OrphanManager<'gc> {
    fn default() -> Self {
        Self {
            orphans: Rc::new(Vec::new()),
            pending: Vec::new(),
            listed: HashMap::new(),
            next_orphan_order: 0,
        }
    }
}

/// If the provided `DisplayObjectWeak` should have frames run, returns
/// Some(clip) with an upgraded `MovieClip`.
/// If this returns `None`, the entry should be removed from the orphan list.
fn valid_orphan<'gc>(
    dobj: DisplayObjectWeak<'gc>,
    mc: &Mutation<'gc>,
) -> Option<DisplayObject<'gc>> {
    dobj.upgrade(mc)
        .filter(|dobj| OrphanManager::is_still_orphan(*dobj))
}
