extern crate futures_watch;

use futures::{future::MapErr, Async, Future, Poll, Stream};
use std::{error, fmt};

use svc;

/// A Service that updates itself as a Watch updates.
#[derive(Clone, Debug)]
pub struct Service<T, M: super::Stack<T>> {
    watch: futures_watch::Watch<T>,
    make: M,
    inner: M::Value,
}

#[derive(Debug)]
pub enum Error<I, M> {
    Stack(M),
    Inner(I),
}

impl<T, M> Service<T, M>
where
    M: super::Stack<T>,
{
    pub fn try(watch: futures_watch::Watch<T>, make: M) -> Result<Self, M::Error> {
        let inner = make.make(&*watch.borrow())?;
        Ok(Self {
            watch,
            make,
            inner,
        })
    }
}

impl<T, M> svc::Service for Service<T, M>
where
    M: super::Stack<T>,
    M::Value: svc::Service,
{
    type Request = <M::Value as svc::Service>::Request;
    type Response = <M::Value as svc::Service>::Response;
    type Error = Error<<M::Value as svc::Service>::Error, M::Error>;
    type Future = MapErr<
        <M::Value as svc::Service>::Future,
        fn(<M::Value as svc::Service>::Error) -> Self::Error,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        // Check to see if the watch has been updated and, if so, rebind the service.
        //
        // `watch.poll()` can't actually fail; so errors are not considered.
        while let Ok(Async::Ready(Some(()))) = self.watch.poll() {
            let target = self.watch.borrow();
            // `inner` is only updated if `target` is valid. The caller may
            // choose to continue using the service or discard as is
            // appropriate.
            self.inner = self.make.make(&*target).map_err(Error::Stack)?;
        }

        self.inner.poll_ready().map_err(Error::Inner)
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req).map_err(Error::Inner)
    }
}

impl<I: fmt::Display, M: fmt::Display> fmt::Display for Error<I, M> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Inner(i) => i.fmt(f),
            Error::Stack(m) => m.fmt(f),
        }
    }
}

impl<I: error::Error, M: error::Error> error::Error for Error<I, M> {}

#[cfg(test)]
mod tests {
    extern crate linkerd2_task as task;
    extern crate tokio;

    use futures::future;
    use self::task::test_util::BlockOnFor;
    use self::tokio::runtime::current_thread::Runtime;
    use std::time::Duration;
    use super::*;
    use svc::Service as _Service;

    const TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn rebind() {
        struct Svc(usize);
        impl svc::Service for Svc {
            type Request = ();
            type Response = usize;
            type Error = ();
            type Future = future::FutureResult<usize, ()>;
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
                rt.block_on_for(TIMEOUT, $svc.call(()))
                    .expect("call")
            };
        }

        struct Stack;
        impl ::Stack<usize> for Stack {
            type Value = Svc;
            type Error = ();
            fn make(&self, n: &usize) -> Result<Svc, ()> {
                Ok(Svc(*n))
            }
        }

        let (watch, mut store) = futures_watch::Watch::new(1);
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
