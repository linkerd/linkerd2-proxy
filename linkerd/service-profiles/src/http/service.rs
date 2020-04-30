//! A stack module that produces a Service that routes requests through alternate
//! middleware configurations
//!
//! As the router's Stack is built, a destination is extracted from the stack's
//! target and it is used to get route profiles from ` GetRoutes` implementation.
//!
//! Each route uses a shared underlying concrete dst .  The concrete dst
//! router picks a concrete dst (NameAddr) from the profile's `dst_overrides` if
//! they exist, or uses the router's target's addr if no `dst_overrides` exist.
//! The concrete dst router uses the concrete dst as the target for the
//! underlying stack.

use super::concrete;
use super::requests::Requests;
use super::{GetRoutes, OverrideDestination, Route, Routes, WithRoute};
use futures::{try_ready, Async, Future, Poll, Stream};
use linkerd2_error::Error;
use linkerd2_stack::{NewService, ProxyService};
use rand::{rngs::SmallRng, SeedableRng};
use tokio::sync::watch;
use tracing::{debug, trace};

#[derive(Clone, Debug)]
pub struct Layer<G, R, O = ()> {
    get_routes: G,
    make_route: R,
    dst_override: O,
    /// This is saved into a field so that the same `Arc`s are used and
    /// cloned, instead of calling `Route::default()` every time.
    default_route: Route,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<G, R, CMake, O = ()> {
    default_route: Route,
    get_routes: G,
    make_route: R,
    make_concrete: CMake,
    dst_override: O,
}

pub struct MakeFuture<T, F, R, CMake, O> {
    future: F,
    inner: Option<Inner<T, R, CMake, O>>,
}

struct Inner<T, R, CMake, O> {
    target: T,
    default_route: Route,
    make_route: R,
    make_concrete: CMake,
    dst_override: O,
}

pub struct Service<T, R, C>
where
    T: WithRoute,
    R: NewService<T::Route>,
{
    profiles: watch::Receiver<Routes>,
    requests: Requests<T, R>,
    concrete: C,
}

pub struct Forward<M>(M);

pub struct Override<M> {
    service: concrete::Service<M>,
    update: concrete::Update,
}

impl<G, R, O> Layer<G, R, O> {
    fn new(get_routes: G, make_route: R, dst_override: O) -> Self {
        Self {
            get_routes,
            make_route,
            dst_override,
            default_route: Route::default(),
        }
    }
}

impl<G, R> Layer<G, R> {
    pub fn without_overrides(get_routes: G, make_route: R) -> Self {
        Self::new(get_routes, make_route, ())
    }
}

impl<G, R> Layer<G, R, SmallRng> {
    pub fn with_overrides(get_routes: G, make_route: R) -> Self {
        Self::new(get_routes, make_route, SmallRng::from_entropy())
    }
}

impl<G, R, C, O> tower::layer::Layer<C> for Layer<G, R, O>
where
    G: Clone,
    R: Clone,
    O: Clone,
{
    type Service = MakeSvc<G, R, C, O>;

    fn layer(&self, make_concrete: C) -> Self::Service {
        MakeSvc {
            make_concrete,
            get_routes: self.get_routes.clone(),
            make_route: self.make_route.clone(),
            default_route: self.default_route.clone(),
            dst_override: self.dst_override.clone(),
        }
    }
}

impl<T, G, R, C> tower::Service<T> for MakeSvc<G, R, C, ()>
where
    T: WithRoute + Clone,
    G: GetRoutes<T>,
    R: NewService<T::Route> + Clone,
    C: Clone,
{
    type Response = Service<T, R, Forward<C>>;
    type Error = Error;
    type Future = MakeFuture<T, G::Future, R, C, ()>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.get_routes.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.get_routes.get_routes(target.clone());

        MakeFuture {
            future,
            inner: Some(Inner {
                target,
                make_route: self.make_route.clone(),
                default_route: self.default_route.clone(),
                make_concrete: self.make_concrete.clone(),
                dst_override: self.dst_override.clone(),
            }),
        }
    }
}

impl<T, G, R, C> tower::Service<T> for MakeSvc<G, R, C, SmallRng>
where
    T: WithRoute + Clone,
    G: GetRoutes<T>,
    R: NewService<T::Route> + Clone,
    C: Clone,
{
    type Response = Service<T, R, Override<C>>;
    type Error = Error;
    type Future = MakeFuture<T, G::Future, R, C, SmallRng>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.get_routes.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.get_routes.get_routes(target.clone());

        MakeFuture {
            future,
            inner: Some(Inner {
                target,
                make_route: self.make_route.clone(),
                default_route: self.default_route.clone(),
                make_concrete: self.make_concrete.clone(),
                dst_override: self.dst_override.clone(),
            }),
        }
    }
}

impl<T, F, R, C> Future for MakeFuture<T, F, R, C, ()>
where
    T: WithRoute + Clone,
    F: Future<Item = watch::Receiver<Routes>>,
    F::Error: Into<Error>,
    R: NewService<T::Route>,
{
    type Item = Service<T, R, Forward<C>>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        trace!("poll");
        let profiles = try_ready!(self.future.poll().map_err(Into::into));

        let Inner {
            target,
            make_route,
            default_route,
            make_concrete,
            dst_override: (),
        } = self.inner.take().unwrap();

        let requests = Requests::new(target.clone(), make_route, default_route);
        let svc = Service {
            profiles,
            requests,
            concrete: Forward(make_concrete),
        };

        trace!("forwarding profile service ready");
        Ok(svc.into())
    }
}

impl<T, F, R, C> Future for MakeFuture<T, F, R, C, SmallRng>
where
    T: WithRoute + Clone,
    F: Future<Item = watch::Receiver<Routes>>,
    F::Error: Into<Error>,
    R: NewService<T::Route> + Clone,
{
    type Item = Service<T, R, Override<C>>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        trace!("poll");
        let profiles = try_ready!(self.future.poll().map_err(Into::into));

        let Inner {
            target,
            make_route,
            default_route,
            make_concrete,
            dst_override: rng,
        } = self.inner.take().unwrap();

        let requests = Requests::new(target.clone(), make_route, default_route);
        let (service, update) = concrete::default(make_concrete, rng);
        let svc = Service {
            profiles,
            requests,
            concrete: Override { service, update },
        };

        trace!("overriding profile service ready");
        Ok(svc.into())
    }
}

mod sealed {
    pub trait PollUpdate {
        fn poll_update(&mut self);
    }
}

impl<T, R, C> sealed::PollUpdate for Service<T, R, Override<C>>
where
    T: WithRoute + Clone,
    R: NewService<T::Route>,
{
    // Drive the profiles stream to notready or completion, capturing the
    // most recent update.
    fn poll_update(&mut self) {
        let mut profile = None;
        while let Some(Async::Ready(Some(update))) = self.profiles.poll().ok() {
            profile = Some(update);
        }

        if let Some(profile) = profile {
            if profile.dst_overrides.is_empty() {
                self.concrete
                    .update
                    .set_forward()
                    .expect("both sides of the concrete updater must be held");
            } else {
                debug!(services = profile.dst_overrides.len(), "updating split");

                self.concrete
                    .update
                    .set_split(profile.dst_overrides)
                    .expect("both sides of the concrete updater must be held");
            }

            debug!(routes = profile.routes.len(), "updating routes");
            self.requests.set_routes(profile.routes);
        }
    }
}

impl<T, R, C> sealed::PollUpdate for Service<T, R, Forward<C>>
where
    T: WithRoute + Clone,
    R: NewService<T::Route>,
{
    // Drive the profiles stream to notready or completion, capturing the
    // most recent update.
    fn poll_update(&mut self) {
        let mut profile = None;
        while let Some(Async::Ready(Some(update))) = self.profiles.poll().ok() {
            profile = Some(update);
        }

        if let Some(profile) = profile {
            debug!(routes = profile.routes.len(), "updating routes");
            self.requests.set_routes(profile.routes);
        }
    }
}

impl<U, T, R, C> tower::Service<U> for Service<T, R, C>
where
    Self: sealed::PollUpdate,
    T: WithRoute + Clone,
    R: NewService<T::Route> + Clone,
    C: tower::Service<U>,
    Requests<T, R>: Clone,
{
    type Response = ProxyService<Requests<T, R>, C::Response>;
    type Error = C::Error;
    type Future = ProxyConcrete<Requests<T, R>, C::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        sealed::PollUpdate::poll_update(self);
        self.concrete.poll_ready()
    }

    fn call(&mut self, target: U) -> Self::Future {
        let proxy = self.requests.clone();
        let future = self.concrete.call(target);
        ProxyConcrete {
            future,
            proxy: Some(proxy),
        }
    }
}

pub struct ProxyConcrete<P, F> {
    proxy: Option<P>,
    future: F,
}

impl<F: Future, P> Future for ProxyConcrete<P, F> {
    type Item = ProxyService<P, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.future.poll());
        let service = ProxyService::new(self.proxy.take().unwrap(), inner);
        Ok(service.into())
    }
}

impl<T, M: tower::Service<T>> tower::Service<T> for Forward<M> {
    type Response = M::Response;
    type Error = M::Error;
    type Future = M::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.0.call(req)
    }
}

impl<T, M> tower::Service<T> for Override<M>
where
    T: OverrideDestination,
    M: tower::Service<T>,
    M::Error: Into<Error>,
{
    type Response = M::Response;
    type Error = Error;
    type Future = <concrete::Service<M> as tower::Service<T>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.service.call(req)
    }
}
