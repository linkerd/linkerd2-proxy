extern crate futures;
#[macro_use]
extern crate log;
extern crate linkerd2_never as never;
extern crate tower_service as svc;

pub mod either;
pub mod layer;
mod map_err;
pub mod map_target;
pub mod phantom_data;
pub mod stack_make_service;
pub mod stack_per_request;
pub mod watch;

pub use self::either::Either;
pub use self::layer::Layer;
pub use self::stack_make_service::StackMakeService;

/// A composable builder.
///
/// A `Stack` attempts to build a `Value` given a `T`-typed _target_. An error is
/// returned iff the target cannot be used to produce a value. Otherwise a value
/// is produced.
///
/// The `Layer` trait provides a mechanism to compose stacks that are generic
/// over another, inner `Stack` type.
pub trait Stack<T> {
    type Value;

    /// Indicates that a given `T` could not be used to produce a `Value`.
    type Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error>;

    /// Wraps this `Stack` with an `L`-typed `Layer` to produce a `Stack<U>`.
    fn push<U, L>(self, layer: L) -> L::Stack
    where
        L: Layer<U, T, Self>,
        Self: Sized,
    {
        layer.bind(self)
    }

    /// Wraps this `Stack` such that errors are altered by `map_err`
    fn map_err<M>(self, map_err: M) -> map_err::Stack<Self, M>
    where
        M: map_err::MapErr<Self::Error>,
        Self: Sized,
    {
        map_err::stack(self, map_err)
    }
}

/// Implements `Stack<T>` for any `T` by cloning a `V`-typed value.
pub mod shared {
    use never::Never;

    pub fn stack<V: Clone>(v: V) -> Stack<V> {
        Stack(v)
    }

    #[derive(Clone, Debug)]
    pub struct Stack<V: Clone>(V);

    impl<T, V: Clone> super::Stack<T> for Stack<V> {
        type Value = V;
        type Error = Never;

        fn make(&self, _: &T) -> Result<V, Never> {
            Ok(self.0.clone())
        }
    }
}
