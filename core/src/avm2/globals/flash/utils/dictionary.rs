use crate::avm2::Error;
use crate::avm2::activation::Activation;
use crate::avm2::parameters::ParametersExt;
use crate::avm2::value::Value;

pub use crate::avm2::object::dictionary_allocator;

/// `Dictionary(weakKeys:Boolean = false)`
///
/// The allocator runs before the constructor, so the weak-key decision is made
/// here — still before any entry can be inserted, which is what lets a
/// dictionary use one kind of object key for its whole life.
pub fn dictionary_initializer<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: &[Value<'gc>],
) -> Result<Value<'gc>, Error<'gc>> {
    if args.get_bool(0)
        && let Some(dictionary) = this.as_object().and_then(|o| o.as_dictionary_object())
    {
        dictionary.set_weak_keys();
    }

    Ok(Value::Undefined)
}
