//! AVM2 object impl for the display hierarchy.

use crate::avm2::Error;
use crate::avm2::activation::Activation;
use crate::avm2::function::FunctionArgs;
use crate::avm2::multiname::Multiname;
use crate::avm2::object::script_object::ScriptObjectData;
use crate::avm2::object::{ClassObject, TObject};
use crate::avm2::value::Value;
use crate::display_object::{DisplayObject, TDisplayObject, TDisplayObjectContainer};
use gc_arena::{Collect, Gc, GcWeak, Mutation};
use ruffle_common::utils::HasPrefixField;
use std::fmt::Debug;
use std::sync::OnceLock;

use crate::display_object::aqw_diagnostics_enabled;

fn child_name_fallback_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        crate::display_object::aqw_env_flag("RUFFLE_AQW_CHILD_NAME_FALLBACK", false)
    })
}

fn is_aqw_avatar_slot(name: &str) -> bool {
    matches!(
        name,
        "head"
            | "chest"
            | "hip"
            | "idlefoot"
            | "frontfoot"
            | "backfoot"
            | "frontshoulder"
            | "backshoulder"
            | "fronthand"
            | "backhand"
            | "frontthigh"
            | "backthigh"
            | "frontshin"
            | "backshin"
            | "robe"
            | "backrobe"
    )
}

#[derive(Clone, Collect, Copy)]
#[collect(no_drop)]
pub struct StageObject<'gc>(pub Gc<'gc, StageObjectData<'gc>>);

#[derive(Clone, Collect, Copy, Debug)]
#[collect(no_drop)]
pub struct StageObjectWeak<'gc>(pub GcWeak<'gc, StageObjectData<'gc>>);

#[derive(Clone, Collect, HasPrefixField)]
#[collect(no_drop)]
#[repr(C, align(8))]
pub struct StageObjectData<'gc> {
    /// The base data common to all AVM2 objects.
    base: ScriptObjectData<'gc>,

    /// The associated display object.
    display_object: DisplayObject<'gc>,
}

impl<'gc> StageObject<'gc> {
    /// Allocate the AVM2 side of a display object intended to be of a given
    /// class's type.
    ///
    /// This function makes no attempt to construct the returned object. You
    /// are responsible for calling the native initializer of the given
    /// class at a later time. Typically, a display object that can contain
    /// movie-constructed children must first allocate itself (using this
    /// function), construct it's children, and then finally initialize itself.
    /// Display objects that do not need to use this flow should use
    /// `for_display_object_childless`.
    pub fn for_display_object(
        mc: &Mutation<'gc>,
        display_object: DisplayObject<'gc>,
        class: ClassObject<'gc>,
    ) -> Self {
        Self(Gc::new(
            mc,
            StageObjectData {
                base: ScriptObjectData::new(class),
                display_object,
            },
        ))
    }

    /// Allocate and construct the AVM2 side of a display object intended to be
    /// of a given class's type.
    ///
    /// This function is intended for display objects that do not have children
    /// and thus do not need to be allocated and initialized in separate phases.
    pub fn for_display_object_childless(
        activation: &mut Activation<'_, 'gc>,
        display_object: DisplayObject<'gc>,
        class: ClassObject<'gc>,
    ) -> Result<Self, Error<'gc>> {
        let this = Self::for_display_object(activation.gc(), display_object, class);

        class.call_init(this.into(), FunctionArgs::empty(), activation)?;

        Ok(this)
    }

    /// Create a `graphics` object for a given display object.
    pub fn graphics(
        activation: &mut Activation<'_, 'gc>,
        display_object: DisplayObject<'gc>,
    ) -> Self {
        // note: for Graphics, there's no need to call init.

        let class = activation.avm2().classes().graphics;
        Self(Gc::new(
            activation.gc(),
            StageObjectData {
                base: ScriptObjectData::new(class),
                display_object,
            },
        ))
    }

    pub fn display_object(self) -> DisplayObject<'gc> {
        self.0.display_object
    }

    fn named_child_property(
        self,
        name: &Multiname<'gc>,
        activation: &mut Activation<'_, 'gc>,
    ) -> Option<Value<'gc>> {
        if !child_name_fallback_enabled() {
            return None;
        }

        if !name.contains_public_namespace() {
            return None;
        }

        let local_name = name.local_name()?;
        let slot_name = local_name.to_utf8_lossy();
        let is_avatar_slot = is_aqw_avatar_slot(&slot_name);
        let container = self.display_object().as_container()?;
        let child = container.child_by_name(&local_name, true).or_else(|| {
            if is_avatar_slot {
                self.display_object().construct_frame(activation.context);
                self.display_object()
                    .as_container()?
                    .child_by_name(&local_name, true)
                    .or_else(|| {
                        self.display_object()
                            .as_container()?
                            .child_by_name(&local_name, false)
                    })
            } else {
                None
            }
        });

        let Some(child) = child else {
            if aqw_diagnostics_enabled() && is_avatar_slot {
                let child_names = container
                    .iter_render_list()
                    .filter_map(|child| child.name().map(|name| name.to_utf8_lossy().into_owned()))
                    .collect::<Vec<_>>()
                    .join(",");
                tracing::warn!(
                    target: "aqw_diag",
                    receiver = ?self.display_object(),
                    property = %slot_name,
                    num_children = container.num_children(),
                    child_names,
                    "AQW avatar named child lookup missed"
                );
            }
            return None;
        };

        if child.object2().is_none() {
            child.construct_frame(activation.context);
        }

        child.object2().map(Value::from)
    }
}

impl<'gc> TObject<'gc> for StageObject<'gc> {
    fn gc_base(&self) -> Gc<'gc, ScriptObjectData<'gc>> {
        HasPrefixField::as_prefix_gc(self.0)
    }

    fn get_property_local(
        self,
        name: &Multiname<'gc>,
        activation: &mut Activation<'_, 'gc>,
    ) -> Result<Value<'gc>, Error<'gc>> {
        match self.base().get_property_local(name, activation) {
            Ok(value @ (Value::Null | Value::Undefined)) => self
                .named_child_property(name, activation)
                .map(Ok)
                .unwrap_or(Ok(value)),
            Ok(value) => Ok(value),
            Err(err) => self
                .named_child_property(name, activation)
                .map(Ok)
                .unwrap_or(Err(err)),
        }
    }
}

impl Debug for StageObject<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("StageObject")
            .field("name", &self.base().class_name())
            // .field("display_object", &self.0.display_object) TODO(moulins)
            .field("ptr", &Gc::as_ptr(self.0))
            .finish()
    }
}
