use std::marker::PhantomData;

// A `Layer ` that optionally wraps an inner `Stack`
#[derive(Debug)]
pub struct Optional<T, N, L> {
    inner: Option<L>,
    _p: PhantomData<fn() -> (T, N)>
}

// === impl Optional ===

impl<T, N, L> From<Option<L>> for Optional<T, N, L>
where
    N: super::Stack<T>,
    L: super::Layer<T, T, N, Value = N::Value, Error = N::Error>,
{
    fn from(inner: Option<L>) -> Self {
        Optional { inner, _p: PhantomData }
    }
}

impl<T, N, L> super::Layer<T, T, N> for Optional<T, N, L>
where
    N: super::Stack<T>,
    L: super::Layer<T, T, N, Value = N::Value, Error = N::Error>,
{
    type Value = <super::Either<N, L::Stack> as super::Stack<T>>::Value;
    type Error = <super::Either<N, L::Stack> as super::Stack<T>>::Error;
    type Stack = super::Either<N, L::Stack>;

    fn bind(&self, next: N) -> Self::Stack {
        match self.inner.as_ref() {
            None => super::Either::A(next),
            Some(ref m) => super::Either::B(m.bind(next)),
        }
    }
}
