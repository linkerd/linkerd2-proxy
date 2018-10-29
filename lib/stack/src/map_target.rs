
pub fn layer<T, M>(map_target: M) -> Layer<M>
where
    M: MapTarget<T>,
{
    Layer(map_target)
}

pub trait MapTarget<T> {
    type Target;

    fn map_target(&self, t: &T) -> Self::Target;
}

#[derive(Clone, Debug)]
pub struct Layer<M>(M);

#[derive(Clone, Debug)]
pub struct Stack<S, M> {
    inner: S,
    map_target: M,
}

impl<T, S, M> super::Layer<T, M::Target, S> for Layer<M>
where
    S: super::Stack<M::Target>,
    M: MapTarget<T> + Clone,
{
    type Value = <Stack<S, M> as super::Stack<T>>::Value;
    type Error = <Stack<S, M> as super::Stack<T>>::Error;
    type Stack = Stack<S, M>;

    fn bind(&self, inner: S) -> Self::Stack {
        Stack {
            inner,
            map_target: self.0.clone(),
        }
    }
}

impl<T, S, M> super::Stack<T> for Stack<S, M>
where
    S: super::Stack<M::Target>,
    M: MapTarget<T>,
{
    type Value = S::Value;
    type Error = S::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        self.inner.make(&self.map_target.map_target(target))
    }
}

impl<F, T, U> MapTarget<T> for F
where
    F: Fn(&T) -> U,
{
    type Target = U;
    fn map_target(&self, t: &T) -> U {
        (self)(t)
    }
}
