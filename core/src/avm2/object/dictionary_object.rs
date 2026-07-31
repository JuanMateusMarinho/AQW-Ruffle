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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

fn aqw_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RUFFLE_AQW_DIAGNOSTICS").is_some())
}

/// How many `Dictionary` instances have been allocated, and how many entries
/// currently live in their *object* space.
///
/// The constructor's `weakKeys` argument is accepted and discarded (see
/// `Dictionary.as`), so every one of these entries pins its key for the
/// lifetime of the dictionary — where the player would have collected the
/// ones whose key became unreachable. Only object keys can be weak; string
/// and numeric keys are ordinary dynamic properties and unaffected. That is
/// what makes the second number the deciding one: if a content that asks for
/// weak keys keeps almost nothing in object space, implementing them cannot
/// reclaim anything, however many dictionaries exist.
static DICTIONARY_INSTANCES: AtomicU64 = AtomicU64::new(0);
static DICTIONARY_OBJECT_KEYS: AtomicI64 = AtomicI64::new(0);

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

    Ok(DictionaryObject(Gc::new(activation.gc(), DictionaryObjectData { base })).into())
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
}

impl<'gc> DictionaryObject<'gc> {
    /// Retrieve a value in the dictionary's object space.
    pub fn get_property_by_object(self, name: Object<'gc>) -> Value<'gc> {
        self.base()
            .values()
            .get(&DynamicKey::Object(name))
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
            .insert(DynamicKey::Object(name), value);
    }

    /// Delete a value from the dictionary's object space.
    pub fn delete_property_by_object(self, name: Object<'gc>, mc: &Mutation<'gc>) {
        let removed = self.base().values_mut(mc).remove(&DynamicKey::Object(name));
        if aqw_diagnostics_enabled() && removed.is_some() {
            DICTIONARY_OBJECT_KEYS.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn has_property_by_object(self, name: Object<'gc>) -> bool {
        self.base().values().contains_key(&DynamicKey::Object(name))
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
