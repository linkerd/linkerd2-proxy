pub fn layer<E, M>(map_err: M) -> Layer<M>
where
    M: MapErr<E>,
{
    Layer(map_err)
}

pub(super) fn stack<T, S, M>(inner: S, map_err: M) -> Stack<S, M>
where
    S: super::Stack<T>,
    M: MapErr<S::Error>,
{
    Stack {
        inner,
        map_err,
    }
}

pub trait MapErr<Input> {
    type Output;

    fn map_err(&self, e: Input) -> Self::Output;
}

#[derive(Clone, Debug)]
pub struct Layer<M>(M);

#[derive(Clone, Debug)]
pub struct Stack<S, M> {
    inner: S,
    map_err: M,
}

impl<T, S, M> super::Layer<T, T, S> for Layer<M>
where
    S: super::Stack<T>,
    M: MapErr<S::Error> + Clone,
{
    type Value = <Stack<S, M> as super::Stack<T>>::Value;
    type Error = <Stack<S, M> as super::Stack<T>>::Error;
    type Stack = Stack<S, M>;

    fn bind(&self, inner: S) -> Self::Stack {
        Stack {
            inner,
            map_err: self.0.clone(),
        }
    }
}

impl<T, S, M> super::Stack<T> for Stack<S, M>
where
    S: super::Stack<T>,
    M: MapErr<S::Error>,
{
    type Value = S::Value;
    type Error = M::Output;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        self.inner.make(target).map_err(|e| self.map_err.map_err(e))
    }
}

impl<F, I, O> MapErr<I> for F
where
    F: Fn(I) -> O,
{
    type Output = O;
    fn map_err(&self, i: I) -> O {
        (self)(i)
    }
}
