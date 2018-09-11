use std::marker::PhantomData;

/// A stackable element.
///
/// Given a `Next`-typed inner value, produces a `Bound`-typed value.
/// This is especially useful for composable types like `MakeClient`s.
pub trait Layer<Next> {
    type Bound;

    /// Produce a `Bound` value from a `Next` value.
    fn bind(&self, next: Next) -> Self::Bound;

    /// Compose this `Layer` with another.
    fn and_then<M>(self, inner: M) -> AndThen<Next, Self, M>
    where
        Self: Layer<M::Bound> + Sized,
        M: Layer<Next>,
    {
        AndThen {
            outer: self,
            inner,
            _p: PhantomData,
        }
    }
}

/// Combines two `Layers` into one layer.
#[derive(Debug, Clone)]
pub struct AndThen<Next, Outer, Inner>
where
    Outer: Layer<Inner::Bound>,
    Inner: Layer<Next>,
{
    outer: Outer,
    inner: Inner,
    // `AndThen` should be Send/Sync independently of `Next`.
    _p: PhantomData<fn() -> Next>,
}

impl<Next, Outer, Inner> Layer<Next> for AndThen<Next, Outer, Inner>
where
    Outer: Layer<Inner::Bound>,
    Inner: Layer<Next>,
{
    type Bound = Outer::Bound;

    fn bind(&self, next: Next) -> Self::Bound {
        self.outer.bind(self.inner.bind(next))
    }
}
