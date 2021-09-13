use crate::{layer, NewService};
pub use tower::util::Either;

#[derive(Clone, Debug)]
pub struct NewEither<L, R> {
    left: L,
    right: R,
}

// === impl NewEither ===

impl<L, R> NewEither<L, R> {
    pub fn new(left: L, right: R) -> Self {
        Self { left, right }
    }
}

impl<L, R: Clone> NewEither<L, R> {
    pub fn layer(right: R) -> impl layer::Layer<L, Service = Self> + Clone {
        layer::mk(move |left| Self::new(left, right.clone()))
    }
}

impl<T, U, L, R> NewService<Either<T, U>> for NewEither<L, R>
where
    L: NewService<T>,
    R: NewService<U>,
{
    type Service = Either<L::Service, R::Service>;

    fn new_service(&self, target: Either<T, U>) -> Self::Service {
        match target {
            Either::A(t) => Either::A(self.left.new_service(t)),
            Either::B(t) => Either::B(self.right.new_service(t)),
        }
    }
}

// === impl Either ===

impl<T, L, R> NewService<T> for Either<L, R>
where
    L: NewService<T>,
    R: NewService<T>,
{
    type Service = Either<L::Service, R::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        match self {
            Either::A(n) => Either::A(n.new_service(target)),
            Either::B(n) => Either::B(n.new_service(target)),
        }
    }
}
