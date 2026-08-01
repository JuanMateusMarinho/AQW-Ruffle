//! Special handling for AVM2 orphan objects

use crate::context::UpdateContext;
use crate::display_object::{DisplayObject, DisplayObjectPtr, DisplayObjectWeak, TDisplayObject};
use gc_arena::{Collect, Mutation};
use std::collections::HashSet;
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
}

/// Bumped whenever something in the orphan list might need a frame pass: a new
/// orphan, or a mark whose walk ended at a parentless object (an orphan root --
/// the stage is excluded, since a mark that reached the stage is about the
/// on-stage tree).
///
/// A nested goto compares this against the value it last processed. If it has
/// not moved, no orphan can have become dirty since, so both orphan loops are
/// skipped whole. Measured 2026-08-01: with the subtree skip working, 95-99% of
/// orphan visits in a nested frame found nothing to do -- 5.2M visits against
/// 50k units of work in one window -- and the list only grows over a session
/// (27 to 1764 orphans), so the waste grows with it.
pub static ORPHAN_DIRTY_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

pub fn bump_orphan_dirty_epoch() {
    ORPHAN_DIRTY_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

impl<'gc> OrphanManager<'gc> {
    fn orphans_mut(&mut self) -> &mut Vec<DisplayObjectWeak<'gc>> {
        // Deliberately does *not* bump the epoch. `cleanup_dead_orphans` runs at
        // the end of every nested goto frame and goes through here, so bumping
        // on any mutation invalidated the epoch ~2000 times a window and the
        // gate never engaged at all. Only *adding* an orphan can create work;
        // removing one can only remove it. So the bump lives in `add_orphan_obj`.
        Rc::make_mut(&mut self.orphans)
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
            // A list the nested goto has already cleared just gained an entry
            // it has never looked at.
            if crate::display_object::aqw_diagnostics_enabled() {
                crate::display_object::AQW_ORPHAN_ADDS
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            bump_orphan_dirty_epoch();
        }
    }

    /// Remove a display object from the orphan list.
    pub fn remove_orphan_obj(&mut self, dobj: DisplayObject<'gc>) {
        self.orphans_mut()
            .retain(|orphan| !std::ptr::eq(orphan.as_ptr(), dobj.as_ptr()));
    }

    pub fn remove_orphan_objs(&mut self, objects: &[DisplayObject<'gc>]) {
        let object_ptrs: HashSet<*const DisplayObjectPtr> =
            objects.iter().map(|object| object.as_ptr()).collect();
        self.orphans_mut()
            .retain(|orphan| !object_ptrs.contains(&orphan.as_ptr()));
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
        self.orphans_mut().retain(|d| {
            if let Some(dobj) = valid_orphan(*d, mc) {
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
            }
        });
    }
}

impl<'gc> Default for OrphanManager<'gc> {
    fn default() -> Self {
        Self {
            orphans: Rc::new(Vec::new()),
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
        .filter(|dobj| dobj.parent().is_none() && !dobj.is_in_detached_aqw_avatar_loader())
}
