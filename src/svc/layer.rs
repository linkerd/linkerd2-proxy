use std::marker::PhantomData;

pub trait Layer<Next> {
    type Error;
    type Bound;

    fn bind(&self, next: Next) -> Self::Bound;

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
    _p: PhantomData<Next>,
}

impl<Next, Outer, Inner> Layer<Next> for AndThen<Next, Outer, Inner>
where
    Outer: Layer<Inner::Bound>,
    Inner: Layer<Next>,
{
    type Error = Outer::Error;
    type Bound = Outer::Bound;

    fn bind(&self, next: Next) -> Self::Bound {
        self.outer.bind(self.inner.bind(next))
    }
}
