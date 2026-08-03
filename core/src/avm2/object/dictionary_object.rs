//! Object representation for `flash.utils.Dictionary`

use crate::avm2::Error;
use crate::avm2::activation::Activation;
use crate::avm2::dynamic_map::DynamicKey;
use crate::avm2::object::script_object::ScriptObjectData;
use crate::avm2::object::{ClassObject, Object, TObject};
use crate::avm2::value::Value;
use crate::string::AvmString;
use core::fmt;
use gc_arena::{Collect, Gc, GcWeak, Mutation};
use ruffle_common::utils::HasPrefixField;
use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::display_object::aqw_diagnostics_enabled;

/// How many `Dictionary` instances have been allocated, and how many entries
/// currently live in their *object* space.
///
/// Only object keys can be weak; string and numeric keys are ordinary dynamic
/// properties and unaffected. That makes the second number the one to watch:
/// it is the population that `weakKeys` governs, so it is both the measure of
/// how much a weak dictionary is holding and the signal that pruning is
/// reclaiming it.
static DICTIONARY_INSTANCES: AtomicU64 = AtomicU64::new(0);
static DICTIONARY_OBJECT_KEYS: AtomicI64 = AtomicI64::new(0);

/// Insertions into a weak dictionary between sweeps for dead keys.
///
/// A weak dictionary that stays a constant size while its keys die would never
/// trip a growth-based trigger, so the schedule counts writes rather than
/// length. One sweep per this many insertions keeps it amortized.
const PRUNE_INTERVAL: usize = 64;

/// `(instances allocated, live object-space entries)`. Both are only tracked
/// under `RUFFLE_AQW_DIAGNOSTICS`, so they read zero otherwise.
pub fn dictionary_stats() -> (u64, i64) {
    (
        DICTIONARY_INSTANCES.load(Ordering::Relaxed),
        DICTIONARY_OBJECT_KEYS.load(Ordering::Relaxed),
    )
}

/// A class instance allocator that allocates Dictionary objects.
pub fn dictionary_allocator<'gc>(
    class: ClassObject<'gc>,
    activation: &mut Activation<'_, 'gc>,
) -> Result<Object<'gc>, Error<'gc>> {
    let base = ScriptObjectData::new(class);

    if aqw_diagnostics_enabled() {
        DICTIONARY_INSTANCES.fetch_add(1, Ordering::Relaxed);
    }

    Ok(DictionaryObject(Gc::new(
        activation.gc(),
        DictionaryObjectData {
            base,
            weak_keys: Cell::new(false),
            inserts_since_prune: Cell::new(0),
        },
    ))
    .into())
}

/// An object that allows associations between objects and values.
///
/// This is implemented by way of "object space", parallel to the property
/// space that ordinary properties live in. This space has no namespaces, and
/// keys are objects instead of strings.
#[derive(Clone, Collect, Copy)]
#[collect(no_drop)]
pub struct DictionaryObject<'gc>(pub Gc<'gc, DictionaryObjectData<'gc>>);

#[derive(Clone, Collect, Copy, Debug)]
#[collect(no_drop)]
pub struct DictionaryObjectWeak<'gc>(pub GcWeak<'gc, DictionaryObjectData<'gc>>);

impl fmt::Debug for DictionaryObject<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DictionaryObject")
            .field("ptr", &Gc::as_ptr(self.0))
            .finish()
    }
}

#[derive(Clone, Collect, HasPrefixField)]
#[collect(no_drop)]
#[repr(C, align(8))]
pub struct DictionaryObjectData<'gc> {
    /// Base script object
    base: ScriptObjectData<'gc>,

    /// Whether `new Dictionary(true)` asked for weak keys, in which case the
    /// object space is keyed by `DynamicKey::WeakObject` and an entry no
    /// longer keeps its own key alive.
    #[collect(require_static)]
    weak_keys: Cell<bool>,

    /// Insertions since the last sweep for collected keys (weak only).
    #[collect(require_static)]
    inserts_since_prune: Cell<usize>,
}

impl<'gc> DictionaryObject<'gc> {
    /// Hold object keys weakly from here on (`new Dictionary(true)`).
    ///
    /// Called from the constructor, before any entry can exist, so there is
    /// never a mix of strong and weak keys in one dictionary.
    pub fn set_weak_keys(self) {
        self.0.weak_keys.set(true);
    }

    /// The key this dictionary files `name` under.
    ///
    /// A live `Object` and its `WeakObject` hash identically (both by box
    /// address), so a lookup built this way finds the entry either way.
    fn object_key(self, name: Object<'gc>) -> DynamicKey<'gc> {
        if self.0.weak_keys.get() {
            DynamicKey::WeakObject(name.downgrade())
        } else {
            DynamicKey::Object(name)
        }
    }

    /// Drop entries whose key has been collected.
    ///
    /// Only enumeration and `length` can observe them -- a lookup always comes
    /// from a live `Object`, whose entry is by definition still live -- so this
    /// is about not letting dead entries accumulate, and about reporting the
    /// object-key population honestly.
    fn prune_dead_keys(self, mc: &Mutation<'gc>) {
        self.0.inserts_since_prune.set(0);

        let pruned = self.base().values_mut(mc).retain(|key| match key {
            // `upgrade` returns `None` both for an already-dropped pointer and
            // for one the in-progress sweep has already condemned. Either way
            // the entry is dead and safe to drop.
            DynamicKey::WeakObject(weak) => weak.upgrade(mc).is_some(),
            _ => true,
        });

        if aqw_diagnostics_enabled() && pruned > 0 {
            DICTIONARY_OBJECT_KEYS.fetch_sub(pruned as i64, Ordering::Relaxed);
        }
    }

    /// Retrieve a value in the dictionary's object space.
    pub fn get_property_by_object(self, name: Object<'gc>) -> Value<'gc> {
        self.base()
            .values()
            .get(&self.object_key(name))
            .map(|v| v.value)
            .unwrap_or(Value::Undefined)
    }

    /// Set a value in the dictionary's object space.
    pub fn set_property_by_object(self, name: Object<'gc>, value: Value<'gc>, mc: &Mutation<'gc>) {
        // Overwriting an existing key is not a new entry, so the probe has to
        // ask first -- `insert` reports nothing back.
        if aqw_diagnostics_enabled() && !self.has_property_by_object(name) {
            DICTIONARY_OBJECT_KEYS.fetch_add(1, Ordering::Relaxed);
        }
        self.base()
            .values_mut(mc)
            .insert(self.object_key(name), value);

        if self.0.weak_keys.get() {
            let writes = self.0.inserts_since_prune.get() + 1;
            self.0.inserts_since_prune.set(writes);
            if writes >= PRUNE_INTERVAL {
                self.prune_dead_keys(mc);
            }
        }
    }

    /// Delete a value from the dictionary's object space.
    pub fn delete_property_by_object(self, name: Object<'gc>, mc: &Mutation<'gc>) {
        let removed = self.base().values_mut(mc).remove(&self.object_key(name));
        if aqw_diagnostics_enabled() && removed.is_some() {
            DICTIONARY_OBJECT_KEYS.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn has_property_by_object(self, name: Object<'gc>) -> bool {
        self.base().values().contains_key(&self.object_key(name))
    }

    /// Whether the entry at `index` is a weak key that has been collected.
    fn enumerant_is_dead(self, index: u32, mc: &Mutation<'gc>) -> bool {
        match self.base().values().key_at(index as usize) {
            Some(DynamicKey::WeakObject(weak)) => weak.upgrade(mc).is_none(),
            _ => false,
        }
    }
}

impl<'gc> TObject<'gc> for DictionaryObject<'gc> {
    fn gc_base(&self) -> Gc<'gc, ScriptObjectData<'gc>> {
        HasPrefixField::as_prefix_gc(self.0)
    }

    // Calling `setPropertyIsEnumerable` on a `Dictionary` has no effect -
    // stringified properties are always enumerable.
    fn set_local_property_is_enumerable(
        &self,
        _mc: &Mutation<'gc>,
        _name: AvmString<'gc>,
        _is_enumerable: bool,
    ) {
    }

    // A collected weak key stays in the table until the next prune. Content
    // must never be handed one, so enumeration steps over them.
    fn get_next_enumerant(
        self,
        last_index: u32,
        activation: &mut Activation<'_, 'gc>,
    ) -> Result<u32, Error<'gc>> {
        let mut index = last_index;
        loop {
            index = self.base().get_next_enumerant(index);
            if index == 0 || !self.enumerant_is_dead(index, activation.gc()) {
                return Ok(index);
            }
        }
    }

    fn get_enumerant_name(
        self,
        index: u32,
        activation: &mut Activation<'_, 'gc>,
    ) -> Result<Value<'gc>, Error<'gc>> {
        if let Some(DynamicKey::WeakObject(weak)) = self.base().values().key_at(index as usize) {
            return Ok(weak
                .upgrade(activation.gc())
                .map_or(Value::Undefined, Value::Object));
        }

        Ok(self.base().get_enumerant_name(index).unwrap_or(Value::Null))
    }

    fn get_enumerant_value(
        self,
        index: u32,
        _activation: &mut Activation<'_, 'gc>,
    ) -> Result<Value<'gc>, Error<'gc>> {
        Ok(*self
            .base()
            .values()
            .value_at(index as usize)
            .unwrap_or(&Value::Undefined))
    }
}
