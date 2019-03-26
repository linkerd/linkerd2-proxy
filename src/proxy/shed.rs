//! Load shedding! Not :bikeshed:.

extern crate tower_load_shed;

use svc;

pub use self::tower_load_shed::error;
use self::tower_load_shed::LoadShed;

#[derive(Clone, Debug)]
pub struct Layer;

#[derive(Clone, Debug)]
pub struct Stack<M>(M);

// === impl Layer ===

pub fn layer() -> Layer {
    Layer
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    M: svc::Stack<T>,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack(inner)
    }
}

// === impl Stack ===

impl<T, M> svc::Stack<T> for Stack<M>
where
    M: svc::Stack<T>,
{
    type Value = LoadShed<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.0.make(target)?;
        Ok(LoadShed::new(inner))
    }
}
