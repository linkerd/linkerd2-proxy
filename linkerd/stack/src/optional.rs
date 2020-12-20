use crate::{layer, Either, NewService};

#[derive(Clone, Debug, Default)]
pub struct NewOptional<A, B> {
    a: A,
    b: B,
}

// === impl NewOptional ===

impl<A, B> NewOptional<A, B> {
    /// Creates a new `ServeHttp`.
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }

    pub fn layer(b: B) -> impl layer::Layer<A, Service = Self>
    where
        B: Clone,
    {
        layer::mk(move |a| Self::new(a, b.clone()))
    }
}

impl<T, U, A, B> NewService<(Option<T>, U)> for NewOptional<A, B>
where
    A: NewService<(T, U)> + Clone,
    B: NewService<U> + Clone,
{
    type Service = Either<A::Service, B::Service>;

    fn new_service(&mut self, (t, u): (Option<T>, U)) -> Self::Service {
        match t {
            Some(t) => Either::A(self.a.new_service((t, u))),
            None => Either::B(self.b.new_service(u)),
        }
    }
}
