extern crate tower_balance;
extern crate tower_discover;
extern crate tower_h2_balance;

use futures::{Async, Poll};
use http;
use std::{error, fmt};
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;
use tower_h2::Body;

pub use self::tower_balance::{choose::PowerOfTwoChoices, load::WithPeakEwma, Balance};
use self::tower_discover::{Change, Discover};
pub use self::tower_h2_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};

use proxy::resolve::{Resolve, Resolution, Update};
use svc;

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Clone, Debug)]
pub struct Layer<T, R, M>  {
    decay: Duration,
    resolve: R,
    _p: PhantomData<fn() -> (T, M)>,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
#[derive(Clone, Debug)]
pub struct Stack<T, R, M> {
    decay: Duration,
    resolve: R,
    inner: M,
    _p: PhantomData<fn() -> T>,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
pub struct Service<R: Resolution, M: svc::Stack<R::Endpoint>> {
    resolution: R,
    make: M,
}

// === impl Layer ===

impl<T, R, M, A, B> Layer<T, R, M>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint> + Clone,
    M::Value: svc::Service<
        Request = http::Request<A>,
        Response = http::Response<B>,
    >,
    A: Body,
    B: Body,
{
    pub const DEFAULT_DECAY: Duration = Duration::from_secs(10);

    pub fn new(resolve: R) -> Self {
        Self {
            resolve,
            decay: Self::DEFAULT_DECAY,
            _p: PhantomData,
        }
    }

    // pub fn with_decay(self, decay: Duration) -> Self {
    //     Self {
    //         decay,
    //         .. self
    //     }
    // }
}

impl<T, R, M, A, B> svc::Layer<T, R::Endpoint, M> for Layer<T, R, M>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint> + Clone,
    M::Value: svc::Service<
        Request = http::Request<A>,
        Response = http::Response<B>,
    >,
    A: Body,
    B: Body,
{
    type Value = <Stack<T, R, M> as svc::Stack<T>>::Value;
    type Error = <Stack<T, R, M> as svc::Stack<T>>::Error;
    type Stack = Stack<T, R, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            decay: self.decay,
            resolve: self.resolve.clone(),
            inner,
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<T, R, M, A, B> svc::Stack<T> for Stack<T, R, M>
where
    R: Resolve<T>,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint> + Clone,
    M::Value: svc::Service<
        Request = http::Request<A>,
        Response = http::Response<B>,
    >,
    A: Body,
    B: Body,
{
    type Value = Balance<
        WithPeakEwma<Service<R::Resolution, M>, PendingUntilFirstData>,
        PowerOfTwoChoices,
    >;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let discover = Service {
            resolution: self.resolve.resolve(&target),
            make: self.inner.clone(),
        };

        let instrument = PendingUntilFirstData::default();
        let loaded = WithPeakEwma::new(discover, self.decay, instrument);
        Ok(Balance::p2c(loaded))
    }
}

// === impl Service ===

impl<R, M> Discover for Service<R, M>
where
    R: Resolution,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint>,
    M::Value: svc::Service,
{
    type Key = SocketAddr;
    type Request = <M::Value as svc::Service>::Request;
    type Response = <M::Value as svc::Service>::Response;
    type Error = <M::Value as svc::Service>::Error;
    type Service = M::Value;
    type DiscoverError = Error<R::Error, M::Error>;

    fn poll(&mut self)
        -> Poll<Change<Self::Key, Self::Service>, Self::DiscoverError>
    {
        loop {
            let up = try_ready!(self.resolution.poll().map_err(Error::Resolve));
            trace!("watch: {:?}", up);
            match up {
                Update::Add(addr, target) => {
                    // We expect the load balancer to handle duplicate inserts
                    // by replacing the old endpoint with the new one, so
                    // insertions of new endpoints and metadata changes for
                    // existing ones can be handled in the same way.
                    let svc = self.make.make(&target).map_err(Error::Stack)?;
                    return Ok(Async::Ready(Change::Insert(addr, svc)));
                },
                Update::Remove(addr) => {
                    return Ok(Async::Ready(Change::Remove(addr)));
                },
            }
        }
    }
}

// === impl Error ===

#[derive(Debug)]
pub enum Error<R, M> {
    Resolve(R),
    Stack(M),
}

impl<M> fmt::Display for Error<(), M>
where
    M: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Resolve(()) => unreachable!("resolution must succeed"),
            Error::Stack(e) => e.fmt(f),
        }
    }
}

impl<M> error::Error for Error<(), M>
where
    M: error::Error,
{
}
