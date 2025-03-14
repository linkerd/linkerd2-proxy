use crate::{Either, Filter, NewEither, Predicate};
use linkerd_error::Error;

pub struct UnwrapOr<U = ()>(std::marker::PhantomData<fn() -> U>);

// === impl Unwrap ===

impl<U> UnwrapOr<U> {
    pub fn layer<N, F>(
        fallback: F,
    ) -> impl super::layer::Layer<N, Service = Filter<NewEither<N, F>, Self>> + Clone
    where
        F: Clone,
    {
        super::layer::mk(move |primary| {
            Filter::new(
                NewEither::new(primary, fallback.clone()),
                Self(std::marker::PhantomData),
            )
        })
    }
}

impl<T, U> Predicate<(Option<T>, U)> for UnwrapOr<U> {
    type Request = Either<(T, U), U>;

    fn check(&mut self, (t, u): (Option<T>, U)) -> Result<Either<(T, U), U>, Error> {
        match t {
            Some(t) => Ok(Either::Left((t, u))),
            None => Ok(Either::Right(u)),
        }
    }
}

impl<T, U: Default> Predicate<Option<T>> for UnwrapOr<U> {
    type Request = Either<T, U>;

    fn check(&mut self, t: Option<T>) -> Result<Either<T, U>, Error> {
        Ok(t.map(Either::Left)
            .unwrap_or_else(|| Either::Right(U::default())))
    }
}

impl<U> Clone for UnwrapOr<U> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<U> Copy for UnwrapOr<U> {}

impl<U> std::fmt::Debug for UnwrapOr<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnwrapOr").finish()
    }
}
