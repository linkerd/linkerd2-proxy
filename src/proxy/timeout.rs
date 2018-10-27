// TODO move to `timeout` crate.

use std::time::Duration;

use svc;
pub use timeout::Timeout;

#[derive(Clone, Debug)]
pub struct Layer {
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
    timeout: Duration,
}

pub fn layer(timeout: Duration) -> Layer {
    Layer { timeout }
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    M: svc::Stack<T>,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            timeout: self.timeout,
        }
    }
}

impl<T, M> svc::Stack<T> for Stack<M>
where
    M: svc::Stack<T>,
{
    type Value = Timeout<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        Ok(Timeout::new(inner, self.timeout))
    }
}
