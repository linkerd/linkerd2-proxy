use crate::{layer, NewService};

// pub use tower::util::Either;
pub use self::vendor::Either;
mod vendor;

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
            Either::Left(t) => Either::Left(self.left.new_service(t)),
            Either::Right(t) => Either::Right(self.right.new_service(t)),
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
            Either::Left(n) => Either::Left(n.new_service(target)),
            Either::Right(n) => Either::Right(n.new_service(target)),
        }
    }
}
