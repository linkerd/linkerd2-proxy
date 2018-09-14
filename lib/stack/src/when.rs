use std::marker::PhantomData;

/// Produces `Stack`s that include an `L`-typed layer only when a `P`-typed
/// `Predicate` evaluates to `true` for the stack's target.
pub struct Layer<T, P, N, L>
where
    P: Predicate<T> + Clone,
    L: super::Layer<T, T, N, Error = N::Error> + Clone,
    N: super::Stack<T>,
{
    predicate: P,
    inner: L,
    _p: PhantomData<(T, N, L)>,
}

/// A condition that determines whether a layer should be applied for a `T`-typed
/// target.
pub trait Predicate<T> {
    /// Returns true iff a conditional layer should be applied for this target.
    fn affirm(&self, target: &T) -> bool;
}

/// When the `P`-typed predicate applies to a target, the `L`
/// `Predicate` evaluates to `true` for the stack's target.
pub struct Stack<T, P, N, L>
where
    P: Predicate<T> + Clone,
    N: super::Stack<T> + Clone,
    L: super::Layer<T, T, N>,
{
    predicate: P,
    next: N,
    layer: L,
    _p: PhantomData<T>,
}

// === impl Layer ===

impl<T, P, N, L> Layer<T, P, N, L>
where
    P: Predicate<T> + Clone,
    N: super::Stack<T> + Clone,
    L: super::Layer<T, T, N, Error = N::Error> + Clone,
    L::Stack: super::Stack<T>,
{
    pub fn new(predicate: P, inner: L) -> Self {
        Self  {
            predicate,
            inner,
            _p: PhantomData,
        }
    }
}

impl<T, P, N, L> Clone for Layer<T, P, N, L>
where
    P: Predicate<T> + Clone,
    N: super::Stack<T> + Clone,
    L: super::Layer<T, T, N, Error = N::Error> + Clone,
    L::Stack: super::Stack<T>,
{
    fn clone(&self) -> Self {
        Self::new(self.predicate.clone(), self.inner.clone())
    }
}

impl<T, P, N, L> super::Layer<T, T, N> for Layer<T, P, N, L>
where
    P: Predicate<T> + Clone,
    N: super::Stack<T> + Clone,
    L: super::Layer<T, T, N, Error = N::Error> + Clone,
    L::Stack: super::Stack<T>,
{
    type Value = <Stack<T, P, N, L> as super::Stack<T>>::Value;
    type Error = <Stack<T, P, N, L> as super::Stack<T>>::Error;
    type Stack = Stack<T, P, N, L>;

    fn bind(&self, next: N) -> Self::Stack {
        Stack {
            predicate: self.predicate.clone(),
            next,
            layer: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<T, P, N, L> Clone for Stack<T, P, N, L>
where
    P: Predicate<T> + Clone,
    N: super::Stack<T> + Clone,
    L: super::Layer<T, T, N, Error = N::Error> + Clone,
    L::Stack: super::Stack<T>,
{
    fn clone(&self) -> Self {
        Self {
            predicate: self.predicate.clone(),
            next: self.next.clone(),
            layer: self.layer.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, P, N, L> super::Stack<T> for Stack<T, P, N, L>
where
    P: Predicate<T> + Clone,
    N: super::Stack<T> + Clone,
    L: super::Layer<T, T, N, Error = N::Error>,
    L::Stack: super::Stack<T>,
{
    type Value = super::Either<N::Value, L::Value>;
    type Error = N::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        if !self.predicate.affirm(&target) {
            self.next
                .make(&target)
                .map(super::Either::A)
        } else {
            self.layer
                .bind(self.next.clone())
                .make(&target)
                .map(super::Either::B)
        }
    }
}

// === impl Predicate<T> for Fn(&T) -> bool ===

impl<T, F: Fn(&T) -> bool> Predicate<T> for F {
    fn affirm(&self, t: &T) -> bool {
        (self)(t)
    }
}
