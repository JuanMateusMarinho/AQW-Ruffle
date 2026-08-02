//! ActionScript Virtual Machine 2 (AS3) support

use crate::PlayerRuntime;
use crate::avm2::bytearray::ObjectEncoding;
use crate::avm2::class::{AllocatorFn, CustomConstructorFn};
use crate::avm2::e4x::XmlSettings;
use crate::avm2::error::{
    Error1014Type, make_error_1014, make_error_1047, make_error_1107, make_error_2022,
    make_error_2023,
};
use crate::avm2::function::exec;
use crate::avm2::globals::{
    SystemClassDefs, SystemClasses, init_builtin_system_class_defs, init_builtin_system_classes,
    init_native_system_classes,
};
use crate::avm2::method::{Method, NativeMethodImpl};
use crate::avm2::object::FunctionObject;
use crate::avm2::scope::ScopeChain;
use crate::avm2::script::{Script, TranslationUnit};
use crate::avm2::stack::Stack;
use crate::character::Character;
use crate::context::UpdateContext;
use crate::display_object::{DisplayObject, MovieClip, TDisplayObject};
use crate::string::{AvmString, StringContext};
use crate::tag_utils::SwfMovie;

use fnv::FnvHashMap;
use gc_arena::lock::GcRefLock;
use gc_arena::{Collect, Gc, Mutation};
use std::cell::Cell;
use std::collections::HashSet;
use std::sync::Arc;
use swf::DoAbc2Flag;
use swf::avm2::read::Reader;

#[macro_export]
macro_rules! avm_debug {
    ($avm: expr, $($arg:tt)*) => (
        if $avm.show_debug_output() {
            tracing::debug!($($arg)*)
        }
    )
}

pub mod activation;
mod amf;
pub mod api_version;
mod array;
pub mod bytearray;
mod call_stack;
mod class;
mod domain;
mod dynamic_map;
mod e4x;
pub mod error;
mod events;
mod filters;
mod flv;
mod function;
pub mod globals;
mod metadata;
mod method;
mod multiname;
mod namespace;
pub mod object;
mod op;
mod optimizer;
mod parameters;
pub mod property;
mod property_map;
mod qname;
mod regexp;
mod scope;
pub mod script;
#[cfg(feature = "known_stubs")]
pub mod specification;
mod stack;
mod stubs;
mod traits;
mod value;
pub mod vector;
mod verify;
mod vtable;

pub use crate::avm2::activation::Activation;
pub use crate::avm2::array::ArrayStorage;
pub use crate::avm2::call_stack::CallStack;
pub use crate::avm2::class::Class;
#[allow(unused)] // For debug_ui
pub use crate::avm2::domain::{Domain, DomainPtr};
pub use crate::avm2::error::Error;
pub use crate::avm2::flv::FlvValueAvm2Ext;
pub use crate::avm2::function::FunctionArgs;
pub use crate::avm2::globals::flash::ui::context_menu::make_context_menu_state;
pub use crate::avm2::multiname::Multiname;
pub use crate::avm2::namespace::{CommonNamespaces, Namespace};
pub use crate::avm2::object::{
    ArrayObject, BitmapDataObject, ClassObject, EventObject, LoaderInfoObject, Object,
    SharedObjectObject, SoundChannelObject, Stage3DObject, StageObject, TObject,
};
pub use crate::avm2::qname::QName;
pub use crate::avm2::value::Value;

use self::api_version::ApiVersion;
use self::object::WeakObject;
use self::scope::Scope;

const MAX_AVM2_EVENT_RECURSION_DEPTH: u32 = 256;

thread_local! {
    static AVM2_EVENT_RECURSION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct Avm2EventRecursionGuard;

impl Avm2EventRecursionGuard {
    fn enter<'gc>(event_type: AvmString<'gc>, operation: &'static str) -> Option<Self> {
        let current_depth = AVM2_EVENT_RECURSION_DEPTH.with(|depth| {
            let current_depth = depth.get().saturating_add(1);
            depth.set(current_depth);
            current_depth
        });

        if current_depth > MAX_AVM2_EVENT_RECURSION_DEPTH {
            AVM2_EVENT_RECURSION_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
            tracing::error!(
                operation = operation,
                event_type = ?event_type,
                depth = current_depth,
                limit = MAX_AVM2_EVENT_RECURSION_DEPTH,
                "AVM2 event recursion limit exceeded; skipping event dispatch"
            );
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for Avm2EventRecursionGuard {
    fn drop(&mut self) {
        AVM2_EVENT_RECURSION_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

const BROADCAST_WHITELIST: [&[u8]; 4] =
    [b"enterFrame", b"exitFrame", b"frameConstructed", b"render"];

#[derive(Clone, Copy, Collect)]
#[collect(no_drop)]
struct BroadcastListener<'gc> {
    object: WeakObject<'gc>,
    order: u64,
}

/// The state of an AVM2 interpreter.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Avm2<'gc> {
    /// The Flash Player version we're emulating.
    player_version: u8,

    /// The player runtime we're emulating
    #[collect(require_static)]
    pub player_runtime: PlayerRuntime,

    /// Values currently present on the operand stack.
    stack: Stack<'gc>,

    /// Scopes currently present of the scope stack.
    scope_stack: Vec<Scope<'gc>>,

    /// The current call stack of the player.
    call_stack: GcRefLock<'gc, CallStack<'gc>>,

    /// This domain is used exclusively for classes from playerglobals
    playerglobals_domain: Domain<'gc>,

    /// The domain associated with 'stage.loaderInfo.applicationDomain'.
    /// Note that this is a parent of the root movie clip's domain
    /// (which can be observed from ActionScript)
    stage_domain: Domain<'gc>,

    /// System classes.
    system_classes: Option<SystemClasses<'gc>>,

    /// System class definitions.
    system_class_defs: Option<SystemClassDefs<'gc>>,

    /// Top-level global object. It contains most top-level types (Object, Class) and functions.
    /// However, it's not strictly defined which items end up there.
    toplevel_global_object: Option<Object<'gc>>,

    /// Pre-created known namespaces.
    namespaces: Gc<'gc, CommonNamespaces<'gc>>,

    #[collect(require_static)]
    native_method_table: &'static [Option<NativeMethodImpl>],

    #[collect(require_static)]
    native_instance_allocator_table: &'static [Option<AllocatorFn>],

    #[collect(require_static)]
    native_call_handler_table: &'static [Option<NativeMethodImpl>],

    #[collect(require_static)]
    native_custom_constructor_table: &'static [Option<CustomConstructorFn>],

    native_fast_call_list: &'static [usize],

    /// A list of objects which are capable of receiving broadcasts.
    ///
    /// Certain types of events are "broadcast events" that are emitted on all
    /// constructed objects in order of their creation, whether or not they are
    /// currently present on the display list. This list keeps track of that.
    broadcast_list: FnvHashMap<AvmString<'gc>, Vec<BroadcastListener<'gc>>>,

    /// AQW player asset listeners temporarily removed from the active broadcast
    /// lists while their Loader is detached from the display list.
    broadcast_list_suspended: FnvHashMap<AvmString<'gc>, Vec<BroadcastListener<'gc>>>,

    /// Number of AQW avatar loaders currently in the one-frame "detach pending"
    /// window (parent just removed, not yet suspended). While this is zero -
    /// i.e. in any steady-state room - the per-listener detached-loader check in
    /// `broadcast_event` is skipped entirely, so it costs nothing outside of map
    /// transitions. Maintained by `LoaderDisplay`'s detach bookkeeping.
    #[collect(require_static)]
    aqw_pending_detach_count: u32,

    next_broadcast_listener_order: u64,

    alias_to_class_map: FnvHashMap<AvmString<'gc>, ClassObject<'gc>>,
    class_to_alias_map: FnvHashMap<Class<'gc>, AvmString<'gc>>,

    #[collect(require_static)]
    pub xml_settings: XmlSettings,

    pub default_bytearray_encoding: ObjectEncoding,

    /// The api version of our root movie clip. Note - this is used as the
    /// api version for swfs loaded via `Loader`, overriding the api version
    /// specified in the loaded SWF. This is only used for API versioning (hiding
    /// definitions from playerglobals) - version-specific behavior in things like
    /// `gotoAndPlay` uses the current movie clip's SWF version.
    #[collect(require_static)]
    pub root_api_version: ApiVersion,

    #[cfg(feature = "avm_debug")]
    pub debug_output: bool,

    pub optimizer_enabled: bool,
}

impl<'gc> Avm2<'gc> {
    /// Construct a new AVM interpreter.
    pub fn new(
        context: &mut StringContext<'gc>,
        player_version: u8,
        player_runtime: PlayerRuntime,
    ) -> Self {
        let mc = context.gc();

        let playerglobals_domain = Domain::uninitialized_domain(mc, None);
        let stage_domain = Domain::uninitialized_domain(mc, Some(playerglobals_domain));

        let namespaces = CommonNamespaces::new(context);

        Self {
            player_version,
            player_runtime,
            stack: Stack::new(mc),
            scope_stack: Vec::new(),
            call_stack: GcRefLock::new(mc, CallStack::new().into()),
            playerglobals_domain,
            stage_domain,
            system_classes: None,
            system_class_defs: None,
            toplevel_global_object: None,

            namespaces: Gc::new(mc, namespaces),

            native_method_table: Default::default(),
            native_instance_allocator_table: Default::default(),
            native_call_handler_table: Default::default(),
            native_custom_constructor_table: Default::default(),
            native_fast_call_list: Default::default(),
            broadcast_list: Default::default(),
            broadcast_list_suspended: Default::default(),
            aqw_pending_detach_count: 0,
            next_broadcast_listener_order: 0,

            alias_to_class_map: Default::default(),
            class_to_alias_map: Default::default(),

            xml_settings: XmlSettings::new_default(),
            default_bytearray_encoding: ObjectEncoding::Amf3,

            // Set the lowest version for now - this will be overridden when we set our movie
            root_api_version: ApiVersion::AllVersions,

            #[cfg(feature = "avm_debug")]
            debug_output: false,

            optimizer_enabled: true,
        }
    }

    pub fn load_player_globals(context: &mut UpdateContext<'gc>) {
        let globals = context.avm2.playerglobals_domain;
        globals::load_playerglobal(context, globals);
    }

    pub fn playerglobals_domain(&self) -> Domain<'gc> {
        self.playerglobals_domain
    }

    /// Return the current set of system classes.
    ///
    /// This function panics if the interpreter has not yet been initialized.
    pub fn classes(&self) -> &SystemClasses<'gc> {
        self.system_classes.as_ref().unwrap()
    }

    /// Return the current set of system class definitions.
    ///
    /// This function panics if the interpreter has not yet been initialized.
    pub fn class_defs(&self) -> &SystemClassDefs<'gc> {
        self.system_class_defs.as_ref().unwrap()
    }

    pub fn toplevel_global_object(&self) -> Option<Object<'gc>> {
        self.toplevel_global_object
    }

    pub fn register_class_alias(&mut self, name: AvmString<'gc>, class_object: ClassObject<'gc>) {
        self.alias_to_class_map.insert(name, class_object);
        self.class_to_alias_map
            .insert(class_object.inner_class_definition(), name);
    }

    pub fn get_class_by_alias(&self, name: AvmString<'gc>) -> Option<ClassObject<'gc>> {
        self.alias_to_class_map.get(&name).copied()
    }

    pub fn get_alias_by_class(&self, cls: Class<'gc>) -> Option<AvmString<'gc>> {
        self.class_to_alias_map.get(&cls).copied()
    }

    /// Run a script's initializer method.
    #[inline(never)]
    pub fn run_script_initializer(
        script: Script<'gc>,
        context: &mut UpdateContext<'gc>,
    ) -> Result<(), Error<'gc>> {
        let (method, global_object, domain) = script.init();

        let scope = ScopeChain::new(domain);
        // Script `global` classes extend Object
        let bound_superclass = Some(context.avm2.classes().object);

        // Provide a callee object if necessary
        let callee = if method.needs_arguments_object() {
            Some(FunctionObject::from_method(
                context,
                method,
                scope,
                Some(global_object.into()),
                bound_superclass,
            ))
        } else {
            None
        };

        // TODO can we skip creating this temporary Activation?
        let mut activation = Activation::from_nothing(context);

        exec(
            method,
            scope,
            global_object.into(),
            bound_superclass,
            FunctionArgs::empty(),
            &mut activation,
            callee,
        )?;

        Ok(())
    }

    /// Dispatch an event on an object.
    ///
    /// This will become its own self-contained activation and swallow
    /// any resulting error (after logging).
    ///
    /// Attempts to dispatch a non-event object will panic.
    ///
    /// Returns `true` if the event has been handled.
    pub fn dispatch_event(
        context: &mut UpdateContext<'gc>,
        event: EventObject<'gc>,
        target: Object<'gc>,
    ) -> bool {
        Self::dispatch_event_internal(context, event, target, false)
    }

    /// Simulate dispatching an event.
    ///
    /// This method is similar to [`Self::dispatch_event`],
    /// but it does not execute event handlers.
    ///
    /// Returns `true` when the event would have been handled if not simulated.
    pub fn simulate_event_dispatch(
        context: &mut UpdateContext<'gc>,
        event: EventObject<'gc>,
        target: Object<'gc>,
    ) -> bool {
        Self::dispatch_event_internal(context, event, target, true)
    }

    fn dispatch_event_internal(
        context: &mut UpdateContext<'gc>,
        event: EventObject<'gc>,
        target: Object<'gc>,
        simulate_dispatch: bool,
    ) -> bool {
        let event_type = event.event().event_type();
        let Some(_event_guard) =
            Avm2EventRecursionGuard::enter(event_type, "dispatch_event_internal")
        else {
            return false;
        };

        let mut activation = Activation::from_nothing(context);

        events::dispatch_event(&mut activation, target, event, simulate_dispatch)
    }

    /// Add an object to the broadcast list.
    ///
    /// Each broadcastable event contains its own broadcast list. You must
    /// register all objects that have event handlers with that event's
    /// broadcast list by calling this function. Attempting to register a
    /// broadcast listener for a non-broadcast event will do nothing.
    ///
    /// Attempts to register the same listener for the same event will also do
    /// nothing.
    /// `(live listeners, suspended listeners, largest bucket, its event name)`.
    ///
    /// `broadcast_frame_entered` dispatches to the whole `enterFrame` bucket
    /// every frame, and that call sits inside the stage's `enter_frame` -- so
    /// its cost has been landing in `stage_enter_ms` and reading as tree-walk
    /// time, even though it walks listeners rather than display objects.
    pub fn broadcast_stats(&self) -> (usize, usize, usize, String) {
        let live = self.broadcast_list.values().map(Vec::len).sum();
        let suspended = self.broadcast_list_suspended.values().map(Vec::len).sum();
        let (mut max, mut name) = (0, String::new());
        for (event, bucket) in &self.broadcast_list {
            if bucket.len() > max {
                max = bucket.len();
                name = event.to_string();
            }
        }
        (live, suspended, max, name)
    }

    pub fn remove_object_from_broadcast_list(
        &mut self,
        object_ptr: *const crate::avm2::object::ObjectPtr,
    ) {
        for bucket in self.broadcast_list.values_mut() {
            bucket.retain(|listener| !std::ptr::eq(listener.object.as_ptr(), object_ptr));
        }
        for bucket in self.broadcast_list_suspended.values_mut() {
            bucket.retain(|listener| !std::ptr::eq(listener.object.as_ptr(), object_ptr));
        }
    }

    /// Remove one object from a specific broadcast event after its last
    /// listener for that event has been removed.
    pub fn unregister_broadcast_listener(
        &mut self,
        object_ptr: *const crate::avm2::object::ObjectPtr,
        event_name: AvmString<'gc>,
    ) {
        if let Some(bucket) = self.broadcast_list.get_mut(&event_name) {
            bucket.retain(|listener| !std::ptr::eq(listener.object.as_ptr(), object_ptr));
        }
        if let Some(bucket) = self.broadcast_list_suspended.get_mut(&event_name) {
            bucket.retain(|listener| !std::ptr::eq(listener.object.as_ptr(), object_ptr));
        }
    }

    pub fn suspend_objects_from_broadcast_list(
        &mut self,
        object_ptrs: &[*const crate::avm2::object::ObjectPtr],
    ) {
        let object_ptrs: HashSet<_> = object_ptrs.iter().copied().collect();
        let mut suspended = Vec::new();

        for (event_name, bucket) in &mut self.broadcast_list {
            bucket.retain(|listener| {
                if object_ptrs.contains(&listener.object.as_ptr()) {
                    suspended.push((*event_name, *listener));
                    false
                } else {
                    true
                }
            });
        }

        for (event_name, listener) in suspended {
            let bucket = self.broadcast_list_suspended.entry(event_name).or_default();
            if bucket
                .iter()
                .all(|entry| !std::ptr::eq(entry.object.as_ptr(), listener.object.as_ptr()))
            {
                bucket.push(listener);
            }
        }
    }

    pub fn restore_objects_to_broadcast_list(
        &mut self,
        object_ptrs: &[*const crate::avm2::object::ObjectPtr],
    ) {
        let object_ptrs: HashSet<_> = object_ptrs.iter().copied().collect();
        let mut restored = Vec::new();

        for (event_name, bucket) in &mut self.broadcast_list_suspended {
            bucket.retain(|listener| {
                if object_ptrs.contains(&listener.object.as_ptr()) {
                    restored.push((*event_name, *listener));
                    false
                } else {
                    true
                }
            });
        }

        for (event_name, listener) in restored {
            let bucket = self.broadcast_list.entry(event_name).or_default();
            if bucket
                .iter()
                .all(|entry| !std::ptr::eq(entry.object.as_ptr(), listener.object.as_ptr()))
            {
                bucket.push(listener);
                bucket.sort_unstable_by_key(|entry| entry.order);
            }
        }
    }

    pub fn broadcast_listener_count(&self) -> usize {
        self.broadcast_list.values().map(std::vec::Vec::len).sum()
    }

    /// Whether any AQW avatar loader is in the one-frame detach-pending window.
    /// When false, `broadcast_event` can skip its per-listener detached-loader
    /// check entirely.
    pub fn aqw_has_pending_detach(&self) -> bool {
        self.aqw_pending_detach_count > 0
    }

    pub fn aqw_inc_pending_detach(&mut self) {
        self.aqw_pending_detach_count = self.aqw_pending_detach_count.saturating_add(1);
    }

    pub fn aqw_dec_pending_detach(&mut self) {
        self.aqw_pending_detach_count = self.aqw_pending_detach_count.saturating_sub(1);
    }

    pub fn register_broadcast_listener(
        context: &mut UpdateContext<'gc>,
        object: Object<'gc>,
        event_name: AvmString<'gc>,
    ) {
        if !BROADCAST_WHITELIST.iter().any(|x| *x == &event_name) {
            return;
        }

        let suspended = object
            .as_display_object()
            .is_some_and(|display_object| display_object.is_in_detached_aqw_avatar_loader());
        let bucket = if suspended {
            context
                .avm2
                .broadcast_list_suspended
                .entry(event_name)
                .or_default()
        } else {
            context.avm2.broadcast_list.entry(event_name).or_default()
        };

        for entry in bucket.iter() {
            // Note: comparing pointers is correct because GcWeak keeps its allocation alive,
            // so the pointers can't overlap by accident.
            if std::ptr::eq(entry.object.as_ptr(), object.as_ptr()) {
                return;
            }
        }

        let order = context.avm2.next_broadcast_listener_order;
        context.avm2.next_broadcast_listener_order =
            context.avm2.next_broadcast_listener_order.saturating_add(1);
        bucket.push(BroadcastListener {
            object: object.downgrade(),
            order,
        });
    }

    /// Dispatch an event on all objects in the current execution list.
    ///
    /// `on_type` specifies a class or interface constructor whose instances,
    /// implementers, and/or subclasses define the set of objects that will
    /// receive the event. You can broadcast to just display objects, or
    /// specific interfaces, and so on.
    ///
    /// Attempts to broadcast a non-broadcast event will do nothing. To add a
    /// new broadcast type, you must add it to the `BROADCAST_WHITELIST` first.
    ///
    /// Attempts to broadcast a non-event object will panic.
    pub fn broadcast_event(
        context: &mut UpdateContext<'gc>,
        event: EventObject<'gc>,
        on_type: ClassObject<'gc>,
    ) {
        let event_name = event.event().event_type();

        if !BROADCAST_WHITELIST.iter().any(|x| *x == &event_name) {
            return;
        }

        let Some(_event_guard) = Avm2EventRecursionGuard::enter(event_name, "broadcast_event")
        else {
            return;
        };

        context.avm2.broadcast_list.entry(event_name).or_default();

        // Walk by the listener's `order`, not by index. A handler can remove
        // itself (or anything else) from this bucket -- `removeEventListener`
        // drops the last listener for an event through
        // `unregister_broadcast_listener` -- and every entry after the removed
        // one then shifts down, so an index-based walk skips exactly one
        // listener. That is what `avm2/movieclip_displayevents_looping`
        // catches: its `delayed_destroy` unregisters during the `exitFrame`
        // broadcast and the next watcher never hears the event.
        //
        // Orders are assigned from a monotonic counter and every path that
        // touches a bucket preserves ascending order, so a `partition_point`
        // finds the next unvisited entry no matter how the bucket moved.
        // Capturing the counter up front keeps the upstream rule that
        // listeners registered *during* a broadcast do not receive it.
        let order_limit = context.avm2.next_broadcast_listener_order;
        let mut next_order = 0u64;

        loop {
            let listener = {
                let bucket = context.avm2.broadcast_list.get(&event_name).unwrap();
                let index = bucket.partition_point(|entry| entry.order < next_order);
                match bucket.get(index) {
                    Some(entry) if entry.order < order_limit => *entry,
                    _ => break,
                }
            };
            next_order = listener.order + 1;

            if let Some(object) = listener.object.upgrade(context.gc())
                && object.is_of_type(on_type.inner_class_definition())
            {
                // Skip AQW avatar-loader children the instant they're parentless,
                // rather than waiting for the deferred suspend bookkeeping to
                // catch up. Otherwise a listener like `AvatarMC.onEnterFrameWalk`
                // can run with `this.stage == null` for one frame before it's
                // suspended, throwing and leaving the avatar stuck (see
                // `is_in_currently_detached_aqw_avatar_loader`).
                //
                // This only matters during the one-frame detach window; gate the
                // per-listener tree walk on a global counter so it's free in any
                // steady-state (e.g. a crowded room with stable avatars), where no
                // loader is mid-detach.
                let suspended = context.avm2.aqw_has_pending_detach()
                    && object
                        .as_display_object()
                        .is_some_and(|display_object| {
                            display_object.is_in_currently_detached_aqw_avatar_loader()
                        });
                if !suspended {
                    // Attribute the handler's cost to its class. Measured
                    // 2026-08-01: ~23 `enterFrame` listeners account for 96%
                    // of the stage phase, so the expensive ones are few and
                    // naming them is the whole question.
                    let probe = crate::display_object::aqw_diagnostics_enabled()
                        .then(std::time::Instant::now);

                    // Profile only what the handler itself runs. Opening the
                    // window around the whole frame would bury the handler in
                    // the rest of the tick, which is the mistake the listener
                    // timer above already had to correct for.
                    let profiling = crate::display_object::aqw_avm2_profile_enabled();
                    if profiling {
                        crate::display_object::aqw_avm2_profile_push_window();
                    }

                    let mut activation = Activation::from_nothing(context);
                    events::broadcast_event(&mut activation, object, event);

                    if profiling {
                        crate::display_object::aqw_avm2_profile_pop_window();
                    }

                    if let Some(started) = probe {
                        // Record every call, not just slow ones. A 100us floor
                        // made a build whose handlers were merely cheap look
                        // like a build that barely called them, which is a
                        // different diagnosis entirely.
                        let elapsed = started.elapsed().as_nanos() as u64;
                        let name = object.instance_class().name().local_name().to_string();
                        crate::display_object::aqw_note_listener_cost(name, elapsed);
                    }
                }
            }
        }
        // Once we're done iterating, remove dead weak references from the list.
        context
            .avm2
            .broadcast_list
            .entry(event_name)
            .or_default()
            .retain(|listener| listener.object.upgrade(context.gc_context).is_some());
    }

    pub fn lookup_class_for_character(
        activation: &mut Activation<'_, 'gc>,
        movie_clip: MovieClip<'gc>,
        domain: Domain<'gc>,
        name: AvmString<'gc>,
        id: u16,
    ) -> Result<ClassObject<'gc>, Error<'gc>> {
        let movie = movie_clip.movie();

        let class_object = domain
            .get_defined_value_handling_vector(activation, name)?
            .as_object()
            .and_then(|o| o.as_class_object())
            .ok_or_else(|| make_error_1014(activation, Error1014Type::ReferenceError, name))?;

        let class = class_object.inner_class_definition();

        let library = activation.context.library.library_for_movie_mut(movie);
        let character = library.character_by_id(id);

        if let Some(character) = character {
            if matches!(
                character,
                Character::EditText(_)
                    | Character::Graphic(_)
                    | Character::MovieClip(_)
                    | Character::Avm2Button(_)
            ) {
                // The class must extend DisplayObject to ensure that events
                // can properly be dispatched to them
                if !class.has_class_in_chain(activation.avm2().class_defs().display_object) {
                    return Err(make_error_2022(activation, class));
                }
            }
        } else if movie_clip.avm2_class().is_none() {
            // If this ID doesn't correspond to any character, and the MC that
            // we're processing doesn't have an AVM2 class set, then this
            // ClassObject is going to be the class of the MC. Ensure it
            // subclasses Sprite.
            if !class.has_class_in_chain(activation.avm2().class_defs().sprite) {
                return Err(make_error_2023(activation, class));
            }
        }

        Ok(class_object)
    }

    /// Load an ABC file embedded in a `DoAbc` or `DoAbc2` tag.
    pub fn do_abc(
        context: &mut UpdateContext<'gc>,
        data: &[u8],
        name: Option<AvmString<'gc>>,
        flags: DoAbc2Flag,
        domain: Domain<'gc>,
        movie: Arc<SwfMovie>,
    ) -> Result<Option<Script<'gc>>, Error<'gc>> {
        let mut reader = Reader::new(data);
        let abc = match reader.read() {
            Ok(abc) => abc,
            Err(_) => {
                let mut activation = Activation::from_nothing(context);
                return Err(make_error_1107(&mut activation));
            }
        };

        let mut activation = Activation::from_domain(context, domain);
        // Make sure we have the correct domain for code that tries to access it
        // using `activation.domain()`
        activation.set_outer(ScopeChain::new(domain));

        if abc.scripts.is_empty() {
            return Err(make_error_1047(&mut activation));
        }

        let num_scripts = abc.scripts.len();
        let tunit = TranslationUnit::from_abc(abc, domain, name, movie, activation.gc());
        tunit.load_classes(&mut activation)?;
        for i in 0..num_scripts {
            tunit.load_script(i as u32, &mut activation)?;
        }

        if !flags.contains(DoAbc2Flag::LAZY_INITIALIZE) {
            return Ok(Some(tunit.get_script(num_scripts - 1).unwrap()));
        }
        Ok(None)
    }

    /// Load the playerglobal ABC file.
    pub fn load_builtin_abc(
        context: &mut UpdateContext<'gc>,
        data: &[u8],
        domain: Domain<'gc>,
        movie: Arc<SwfMovie>,
    ) {
        let mut reader = Reader::new(data);
        let abc = match reader.read() {
            Ok(abc) => abc,
            Err(_) => panic!("Builtin ABC should be valid"),
        };

        let mut activation = Activation::from_domain(context, domain);
        // Make sure we have the correct domain for code that tries to access its
        // domain using `activation.domain()`
        activation.set_outer(ScopeChain::new(domain));

        let tunit = TranslationUnit::from_abc(abc, domain, None, movie, activation.gc());

        globals::init_early_classes(&mut activation, tunit).expect("Early classes should load");

        // At this point we have everything necessary to load scripts and classes.

        tunit
            .load_classes(&mut activation)
            .expect("Classes should load");

        // These Classes are absolutely critical to the runtime, so make sure
        // we've registered them before anything else.
        init_builtin_system_class_defs(&mut activation);

        // The second script (script #1) is Toplevel.as, and includes important
        // builtin classes such as Namespace, QName, and XML.
        let toplevel_script = tunit
            .load_script(1, &mut activation)
            .expect("Script should load");

        // We intentionally avoid running the script initializer here
        let (_, toplevel_global, _) = toplevel_script.init();

        activation.avm2().toplevel_global_object = Some(toplevel_global);

        // HACK: Replace ScopeChains on the class vtable of `Object` to include
        // the toplevel global.
        let mc = activation.gc();

        let new_scope = ScopeChain::new(tunit.domain());
        let new_scope = new_scope.chain(mc, &[Scope::new(toplevel_global.into())]);

        activation
            .avm2()
            .classes()
            .object
            .vtable()
            .replace_scopes_with(mc, new_scope);

        // The scopes must be correct before we run the script initializer from
        // `init_builtin_system_classes`.
        init_builtin_system_classes(&mut activation);

        // The first script (script #0) is globals.as, and includes other builtin
        // classes that are less critical for the AVM to load.
        tunit
            .load_script(0, &mut activation)
            .expect("Script should load");
        init_native_system_classes(&mut activation);
    }

    pub fn stage_domain(&self) -> Domain<'gc> {
        self.stage_domain
    }

    /// Pushes an executable on the call stack
    pub fn push_call(&self, mc: &Mutation<'gc>, method: Method<'gc>) {
        self.call_stack.borrow_mut(mc).push(method)
    }

    /// Pops an executable off the call stack
    pub fn pop_call(&self, mc: &Mutation<'gc>) {
        self.call_stack.borrow_mut(mc).pop();
    }

    pub fn call_depth(&self) -> usize {
        self.call_stack.borrow().depth()
    }

    pub fn call_stack(&self) -> GcRefLock<'gc, CallStack<'gc>> {
        self.call_stack
    }

    pub fn capture_call_stack(&self) -> CallStack<'gc> {
        self.call_stack.borrow().clone()
    }

    fn push_scope(&mut self, scope: Scope<'gc>) {
        self.scope_stack.push(scope);
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }

    #[cfg(feature = "avm_debug")]
    #[inline]
    pub fn show_debug_output(&self) -> bool {
        self.debug_output
    }

    #[cfg(not(feature = "avm_debug"))]
    pub const fn show_debug_output(&self) -> bool {
        false
    }

    #[cfg(feature = "avm_debug")]
    pub fn set_show_debug_output(&mut self, visible: bool) {
        self.debug_output = visible;
    }

    #[cfg(not(feature = "avm_debug"))]
    pub const fn set_show_debug_output(&self, _visible: bool) {}

    /// Gets the public namespace, versioned based on the current root SWF.
    /// See `AvmCore::findPublicNamespace()`
    /// https://github.com/adobe/avmplus/blob/858d034a3bd3a54d9b70909386435cf4aec81d21/core/AvmCore.cpp#L5809C25-L5809C25
    pub fn find_public_namespace(&self) -> Namespace<'gc> {
        self.namespaces.public_for(self.root_api_version)
    }

    pub fn optimizer_enabled(&self) -> bool {
        self.optimizer_enabled
    }

    pub fn set_optimizer_enabled(&mut self, value: bool) {
        self.optimizer_enabled = value;
    }

    // Report an uncaught AVM2 error.
    // TODO should the `display_object` parameter be optional or not?
    #[cold]
    #[inline(never)]
    pub fn uncaught_error(
        activation: &mut Activation<'_, 'gc>,
        _display_object: Option<DisplayObject<'gc>>,
        error: Error<'gc>,
        extra_info: &str,
    ) {
        // This will print the properly formatted error
        let stringified = error.to_string(activation);
        tracing::error!("{}: {}", extra_info, stringified);

        // TODO: push the error onto `loaderInfo.uncaughtErrorEvents`
    }
}
