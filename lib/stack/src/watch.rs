extern crate futures_watch;

use self::futures_watch::Watch;
use futures::{future::MapErr, Async, Future, Poll, Stream};
use std::fmt;
use std::marker::PhantomData;

use svc;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Implemented by targets that can be updated by a `Watch<U>`
pub trait WithUpdate<U> {
    type Updated;

    fn with_update(&self, update: &U) -> Self::Updated;
}

#[derive(Debug)]
pub struct Layer<T: WithUpdate<U>, U, M> {
    watch: Watch<U>,
    _p: PhantomData<fn() -> (T, M)>,
}

#[derive(Debug)]
pub struct Stack<T: WithUpdate<U>, U, M> {
    watch: Watch<U>,
    inner: M,
    _p: PhantomData<fn() -> T>,
}

/// A Service that updates itself as a Watch updates.
#[derive(Debug)]
pub struct Service<T: WithUpdate<U>, U, M: super::Stack<T::Updated>> {
    watch: Watch<U>,
    target: T,
    stack: M,
    inner: M::Value,
}

/// An error in the `Service` when trying to make a new stack from a watch update.
#[derive(Debug)]
pub struct MakeWatch(Error);

/// A special implemtation of WithUpdate that clones the observed update value.
#[derive(Clone, Debug)]
pub struct CloneUpdate {}

// === impl Layer ===

pub fn layer<T, U, M>(watch: Watch<U>) -> Layer<T, U, M>
where
    T: WithUpdate<U> + Clone,
    M: super::Stack<T::Updated> + Clone,
{
    Layer {
        watch,
        _p: PhantomData,
    }
}

impl<T, U, M> Clone for Layer<T, U, M>
where
    T: WithUpdate<U> + Clone,
    M: super::Stack<T::Updated> + Clone,
{
    fn clone(&self) -> Self {
        layer(self.watch.clone())
    }
}

impl<T, U, M> super::Layer<T, T::Updated, M> for Layer<T, U, M>
where
    T: WithUpdate<U> + Clone,
    M: super::Stack<T::Updated> + Clone,
{
    type Value = <Stack<T, U, M> as super::Stack<T>>::Value;
    type Error = <Stack<T, U, M> as super::Stack<T>>::Error;
    type Stack = Stack<T, U, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            watch: self.watch.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<T: WithUpdate<U>, U, M: Clone> Clone for Stack<T, U, M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            watch: self.watch.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, U, M> super::Stack<T> for Stack<T, U, M>
where
    T: WithUpdate<U> + Clone,
    M: super::Stack<T::Updated> + Clone,
{
    type Value = Service<T, U, M>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self
            .inner
            .make(&target.with_update(&*self.watch.borrow()))?;
        Ok(Service {
            inner,
            watch: self.watch.clone(),
            target: target.clone(),
            stack: self.inner.clone(),
        })
    }
}

// === impl Service ===

impl<T, U, M, Svc, R> svc::Service<R> for Service<T, U, M>
where
    T: WithUpdate<U>,
    M: super::Stack<T::Updated, Value = Svc>,
    M::Error: Into<Error>,
    Svc: svc::Service<R>,
    Svc::Error: Into<Error>,
{
    type Response = <M::Value as svc::Service<R>>::Response;
    type Error = Error;
    type Future = MapErr<
        <M::Value as svc::Service<R>>::Future,
        fn(<M::Value as svc::Service<R>>::Error) -> Self::Error,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        // Check to see if the watch has been updated and, if so, rebind the service.
        //
        // `watch.poll()` can't actually fail; so errors are not considered.
        while let Ok(Async::Ready(Some(()))) = self.watch.poll() {
            let updated = self.target.with_update(&*self.watch.borrow());
            // `inner` is only updated if `updated` is valid. The caller may
            // choose to continue using the service or discard as is
            // appropriate.
            self.inner = self
                .stack
                .make(&updated)
                .map_err(|e| Box::new(MakeWatch(e.into())))?;
        }

        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: R) -> Self::Future {
        self.inner.call(req).map_err(Into::into)
    }
}

impl<U, M> Service<CloneUpdate, U, M>
where
    U: Clone,
    M: super::Stack<U>,
{
    pub fn try(watch: Watch<U>, stack: M) -> Result<Self, M::Error> {
        let inner = stack.make(&*watch.borrow())?;
        Ok(Self {
            inner,
            watch,
            stack,
            target: CloneUpdate {},
        })
    }
}

impl<T, U, M> Clone for Service<T, U, M>
where
    T: WithUpdate<U> + Clone,
    M: super::Stack<T::Updated> + Clone,
    M::Value: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            watch: self.watch.clone(),
            stack: self.stack.clone(),
            target: self.target.clone(),
        }
    }
}

// === impl CloneUpdate ===

impl<U: Clone> WithUpdate<U> for CloneUpdate {
    type Updated = U;

    fn with_update(&self, update: &U) -> U {
        update.clone()
    }
}

// === impl Error ===

impl fmt::Display for MakeWatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "error making stack after watch update: {}", self.0)
    }
}

impl std::error::Error for MakeWatch {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

#[cfg(test)]
mod tests {
    extern crate linkerd2_task as task;
    extern crate tokio;

    use self::task::test_util::BlockOnFor;
    use self::tokio::runtime::current_thread::Runtime;
    use super::*;
    use futures::future;
    use never::Never;
    use std::time::Duration;
    use svc::Service as _Service;

    const TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn rebind() {
        struct Svc(usize);
        impl svc::Service<()> for Svc {
            type Response = usize;
            type Error = Never;
            type Future = future::FutureResult<usize, Self::Error>;
            fn poll_ready(&mut self) -> Poll<(), Self::Error> {
                Ok(().into())
            }
            fn call(&mut self, _: ()) -> Self::Future {
                future::ok(self.0)
            }
        }

        let mut rt = Runtime::new().unwrap();
        macro_rules! assert_ready {
            ($svc:expr) => {
                rt.block_on_for(TIMEOUT, future::poll_fn(|| $svc.poll_ready()))
                    .expect("ready")
            };
        }
        macro_rules! call {
            ($svc:expr) => {
                rt.block_on_for(TIMEOUT, $svc.call(())).expect("call")
            };
        }

        struct Stack;
        impl ::Stack<usize> for Stack {
            type Value = Svc;
            type Error = Never;
            fn make(&self, n: &usize) -> Result<Svc, Never> {
                Ok(Svc(*n))
            }
        }

        let (watch, mut store) = Watch::new(1);
        let mut svc = Service::try(watch, Stack).unwrap();

        assert_ready!(svc);
        assert_eq!(call!(svc), 1);

        assert_ready!(svc);
        assert_eq!(call!(svc), 1);

        store.store(2).expect("store");
        assert_ready!(svc);
        assert_eq!(call!(svc), 2);

        store.store(3).expect("store");
        store.store(4).expect("store");
        assert_ready!(svc);
        assert_eq!(call!(svc), 4);

        drop(store);
        assert_ready!(svc);
        assert_eq!(call!(svc), 4);
    }
}
