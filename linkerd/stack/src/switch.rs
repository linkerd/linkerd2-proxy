//! A middleware that switches between two underlying stacks, depending on the
//! target type.

use crate::Either;

/// Determines whether the primary stack should be used.
pub trait Switch<T> {
    type Left;
    type Right;

    fn switch(&self, target: T) -> Either<Self::Left, Self::Right>;
}

#[derive(Copy, Clone, Debug)]
pub struct Unwrap<U = ()>(std::marker::PhantomData<fn() -> U>);

/// Makes either the primary or fallback stack, as determined by an `S`-typed
/// `Switch`.
#[derive(Clone, Debug)]
pub struct NewSwitch<S, P, F> {
    switch: S,
    primary: P,
    fallback: F,
}

// === impl NewSwitch ===

impl<S, P, F> NewSwitch<S, P, F> {
    pub fn new(switch: S, primary: P, fallback: F) -> Self {
        NewSwitch {
            switch,
            primary,
            fallback,
        }
    }

    pub fn layer(switch: S, fallback: F) -> impl super::layer::Layer<P, Service = Self> + Clone
    where
        S: Clone,
        F: Clone,
    {
        super::layer::mk(move |primary| Self::new(switch.clone(), primary, fallback.clone()))
    }
}

impl<T, S, P, F> super::NewService<T> for NewSwitch<S, P, F>
where
    S: Switch<T>,
    P: super::NewService<S::Left>,
    F: super::NewService<S::Right>,
{
    type Service = Either<P::Service, F::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        match self.switch.switch(target) {
            Either::A(t) => Either::A(self.primary.new_service(t)),
            Either::B(t) => Either::B(self.fallback.new_service(t)),
        }
    }
}

// === impl Switch ===

impl<T, F: Fn(&T) -> bool> Switch<T> for F {
    type Left = T;
    type Right = T;

    fn switch(&self, target: T) -> Either<T, T> {
        if (self)(&target) {
            Either::A(target)
        } else {
            Either::B(target)
        }
    }
}

// === impl Unwrap ===

impl<U> Unwrap<U> {
    pub fn layer<N, F>(
        fallback: F,
    ) -> impl super::layer::Layer<N, Service = NewSwitch<Self, N, F>> + Clone
    where
        F: Clone,
    {
        super::layer::mk(move |primary| {
            NewSwitch::new(Self(std::marker::PhantomData), primary, fallback.clone())
        })
    }
}

impl<T, U> Switch<(Option<T>, U)> for Unwrap<U> {
    type Left = (T, U);
    type Right = U;

    fn switch(&self, (t, u): (Option<T>, U)) -> Either<(T, U), U> {
        match t {
            Some(t) => Either::A((t, u)),
            None => Either::B(u),
        }
    }
}

impl<T, U: Default> Switch<Option<T>> for Unwrap<U> {
    type Left = T;
    type Right = U;

    fn switch(&self, t: Option<T>) -> Either<T, U> {
        t.map(Either::A).unwrap_or_else(|| Either::B(U::default()))
    }
}
