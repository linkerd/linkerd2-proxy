use crate::{layer, Either, NewService};

/// A stack middleware that takes an optional target type and builds an `A`-typed
/// stack if the target is set and a `B`-typed stack if it is not.
#[derive(Clone, Debug, Default)]
pub struct NewUnwrapOr<A, B> {
    new_some: A,
    new_none: B,
}

// === impl NewUnwrapOr ===

impl<A, B> NewUnwrapOr<A, B> {
    pub fn new(new_some: A, new_none: B) -> Self {
        Self { new_some, new_none }
    }

    pub fn layer(new_none: B) -> impl layer::Layer<A, Service = Self>
    where
        B: Clone,
    {
        layer::mk(move |new_some| Self::new(new_some, new_none.clone()))
    }
}

impl<T, U, A, B> NewService<(Option<T>, U)> for NewUnwrapOr<A, B>
where
    A: NewService<(T, U)> + Clone,
    B: NewService<U> + Clone,
{
    type Service = Either<A::Service, B::Service>;

    fn new_service(&mut self, (t, u): (Option<T>, U)) -> Self::Service {
        match t {
            Some(t) => Either::A(self.new_some.new_service((t, u))),
            None => Either::B(self.new_none.new_service(u)),
        }
    }
}
