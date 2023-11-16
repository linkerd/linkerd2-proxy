// Possibly unused, but useful during development.

use crate::{disco_cache::NewCachedDiscover, Error};
use linkerd_error::Recover;
use linkerd_exp_backoff::{ExponentialBackoff, ExponentialBackoffStream};
use std::{
    fmt,
    hash::Hash,
    task::{Context, Poll},
    time::Duration,
};
use tower::{
    layer::util::{Identity, Stack as Pair},
    make::MakeService,
};
pub use tower::{
    layer::Layer, limit::GlobalConcurrencyLimitLayer as ConcurrencyLimitLayer, service_fn as mk,
    spawn_ready::SpawnReady, Service, ServiceExt,
};

pub use crate::proxy::http;
pub use linkerd_idle_cache as idle_cache;
pub use linkerd_reconnect::NewReconnect;
pub use linkerd_router::{self as router, NewOneshotRoute};
pub use linkerd_stack::{self as stack, *};
pub use linkerd_stack_tracing::{GetSpan, NewInstrument, NewInstrumentLayer};

#[derive(Copy, Clone, Debug)]
pub struct AlwaysReconnect(ExponentialBackoff);

pub type BoxHttp<B = http::BoxBody> =
    BoxService<http::Request<B>, http::Response<http::BoxBody>, Error>;

pub type ArcNewHttp<T, B = http::BoxBody> = ArcNewService<T, BoxHttp<B>>;

pub type BoxCloneHttp<B = http::BoxBody> =
    BoxCloneService<http::Request<B>, http::Response<http::BoxBody>, Error>;

pub type ArcNewCloneHttp<T, B = http::BoxBody> = ArcNewService<T, BoxCloneHttp<B>>;

pub type BoxCloneSyncHttp<B = http::BoxBody> =
    BoxCloneSyncService<http::Request<B>, http::Response<http::BoxBody>>;

pub type ArcNewCloneSyncHttp<T, B = http::BoxBody> = ArcNewService<T, BoxCloneSyncHttp<B>>;

pub type BoxTcp<I> = BoxService<I, (), Error>;

pub type ArcNewTcp<T, I> = ArcNewService<T, BoxTcp<I>>;

pub type BoxCloneTcp<I> = BoxCloneService<I, (), Error>;

pub type ArcNewCloneTcp<T, I> = ArcNewService<T, BoxCloneTcp<I>>;

#[derive(Clone, Debug)]
pub struct Layers<L>(L);

#[derive(Clone, Debug)]
pub struct Stack<S>(S);

pub fn layers() -> Layers<Identity> {
    Layers(Identity::new())
}

pub fn stack<S>(inner: S) -> Stack<S> {
    Stack(inner)
}

// === impl IdentityProxy ===

pub fn proxies() -> Stack<IdentityProxy> {
    Stack(IdentityProxy(()))
}

#[derive(Copy, Clone, Debug)]
pub struct IdentityProxy(());

impl<T> NewService<T> for IdentityProxy {
    type Service = ();
    fn new_service(&self, _: T) -> Self::Service {}
}

// === impl Layers ===

#[allow(dead_code)]
impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    pub fn push_map_target<M>(self, map_target: M) -> Layers<Pair<L, stack::MapTargetLayer<M>>> {
        self.push(stack::MapTargetLayer::new(map_target))
    }

    pub fn push_on_service<U>(self, layer: U) -> Layers<Pair<L, stack::OnServiceLayer<U>>> {
        self.push(stack::OnServiceLayer::new(layer))
    }

    /// Wraps the inner `N` with `NewCloneService` so that the stack holds a
    /// `NewService` that always returns a clone of `N` regardless of the target
    /// value.
    pub fn lift_new<N>(
        self,
    ) -> Layers<Pair<L, impl Layer<N, Service = NewCloneService<N>> + Clone>> {
        self.push(layer::mk(NewCloneService::from))
    }

    // Wraps the inner `N`-typed [`NewService`] with a layer that applies the
    // given target to all inner stacks to produce its service.
    pub fn push_flatten_new<T, N>(
        self,
        target: T,
    ) -> Layers<Pair<L, impl Layer<N, Service = N::Service> + Clone>>
    where
        T: Clone,
        N: NewService<T>,
    {
        self.push(layer::mk(move |inner: N| inner.new_service(target.clone())))
    }

    pub fn push_instrument<G: Clone>(self, get_span: G) -> Layers<Pair<L, NewInstrumentLayer<G>>> {
        self.push(NewInstrumentLayer::new(get_span))
    }
}

impl<M, L: Layer<M>> Layer<M> for Layers<L> {
    type Service = L::Service;

    fn layer(&self, inner: M) -> Self::Service {
        self.0.layer(inner)
    }
}

// === impl Stack ===

#[allow(dead_code)]
impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    pub fn push_map_target<M: Clone>(self, map_target: M) -> Stack<stack::MapTargetService<S, M>> {
        self.push(stack::MapTargetLayer::new(map_target))
    }

    pub fn push_filter<F: Clone>(self, filter: F) -> Stack<stack::Filter<S, F>> {
        self.push(stack::Filter::<S, F>::layer(filter))
    }

    /// Wraps a `Service<T>` as a `Service<()>`.
    ///
    /// Each time the service is called, the `T`-typed request is cloned and
    /// issued into the inner service.
    pub fn push_new_thunk(self) -> Stack<stack::NewThunk<S>> {
        self.push(layer::mk(stack::NewThunk::new))
    }

    pub fn instrument<G: Clone>(self, get_span: G) -> Stack<NewInstrument<G, S>> {
        self.push(NewInstrumentLayer::new(get_span))
    }

    pub fn instrument_from_target(self) -> Stack<NewInstrument<(), S>> {
        self.push(NewInstrumentLayer::from_target())
    }

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn into_new_service(self) -> Stack<stack::new_service::FromMakeService<S>> {
        self.push(stack::new_service::FromMakeService::layer())
    }

    pub fn push_new_reconnect(
        self,
        backoff: ExponentialBackoff,
    ) -> Stack<NewReconnect<AlwaysReconnect, S>> {
        self.push(NewReconnect::layer(AlwaysReconnect(backoff)))
    }

    /// Assuming `S` implements `NewService` or `MakeService`, applies the given
    /// `L`-typed layer on each service produced by `S`.
    pub fn push_on_service<L: Clone>(self, layer: L) -> Stack<stack::OnService<L, S>> {
        self.push(stack::OnServiceLayer::new(layer))
    }

    /// Lifts `S` into `NewService<_, Service = S>` so that the inner stack is
    /// cloned for each target (ignoring its value).
    pub fn lift_new(self) -> Stack<NewCloneService<S>> {
        self.push(layer::mk(NewCloneService::from))
    }

    /// Lifts the inner stack via [`Self::lift_new`], but combines both target
    /// types into a new `P`-typed value via `From`.
    pub fn lift_new_with_target<P>(self) -> Stack<NewFromTargets<P, NewCloneService<S>>> {
        // `lift_new` takes a NewService<P> and returns a NewService<T, Service =
        // NewService<P>> -- turning it into a double-NewService that ignores
        // the `T`-typed target.
        //
        // `NewFromTargets` wraps that and returns a NewService<T, Service =
        // NewService<(U, T)> -- so that first target is cloned and
        // combined with the second target via `P::from`.
        //
        // The result is that we expose NewService<T, Service = NewService<U>>
        // over an inner NewService<P>.
        self.lift_new().push(NewFromTargets::layer())
    }

    /// Converts an inner `NewService<T, Service = Svc>` into a `NewService<T,
    /// Service = NewService<_, Service = Svc>>`, cloning `Svc` for each child
    /// target (ignoring its value), in effect disarding the child `NewService`.
    ///
    /// The inverse of `lift_new`.
    pub fn unlift_new<Svc>(
        self,
    ) -> Stack<OnService<impl Layer<Svc, Service = NewCloneService<Svc>> + Clone, S>> {
        self.push_on_service(layer::mk(NewCloneService::from))
    }

    /// Wraps the inner service with a response timeout such that timeout errors are surfaced as a
    /// `ConnectTimeout` error.
    ///
    /// Note that any timeouts errors from the inner service will be wrapped as well.
    pub fn push_connect_timeout(
        self,
        timeout: Duration,
    ) -> Stack<MapErr<impl FnOnce(Error) -> Error + Clone, stack::Timeout<S>>> {
        self.push(stack::Timeout::layer(timeout))
            .push(MapErr::layer(move |err: Error| {
                if err.is::<stack::TimeoutError>() {
                    crate::errors::ConnectTimeout(timeout).into()
                } else {
                    err
                }
            }))
    }

    pub fn push_http_insert_target<P>(self) -> Stack<http::insert::NewInsert<P, S>> {
        self.push(http::insert::NewInsert::layer())
    }

    pub fn push_http_response_insert_target<P>(
        self,
    ) -> Stack<http::insert::NewResponseInsert<P, S>> {
        self.push(http::insert::NewResponseInsert::layer())
    }

    pub fn push_new_idle_cached<T>(self, idle: Duration) -> Stack<idle_cache::NewIdleCached<T, S>>
    where
        T: Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
        S: NewService<T> + 'static,
        S::Service: Send + Sync + 'static,
    {
        self.push(idle_cache::NewIdleCached::layer(idle))
    }

    /// Push a service that either calls the inner service if it is ready, or
    /// calls a `secondary` service if the inner service fails to become ready
    /// for the `skip_after` duration.
    pub fn push_when_unready<B: Clone>(
        self,
        skip_after: Duration,
        secondary: B,
    ) -> Stack<stack::NewSwitchReady<S, B>> {
        self.push(layer::mk(|inner: S| {
            stack::NewSwitchReady::new(inner, secondary.clone(), skip_after)
        }))
    }

    pub fn push_switch<P: Clone, U: Clone>(
        self,
        predicate: P,
        other: U,
    ) -> Stack<Filter<stack::NewEither<S, U>, P>> {
        self.push(layer::mk(move |inner| {
            stack::Filter::new(
                stack::NewEither::new(inner, other.clone()),
                predicate.clone(),
            )
        }))
    }

    pub fn push_new_cached_discover<K, D>(
        self,
        discover: D,
        idle: Duration,
    ) -> Stack<NewCachedDiscover<K, D, S>>
    where
        K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
        D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
        D::Response: Clone + Send + Sync + 'static,
        D::Future: Send + Unpin,
    {
        self.push(NewCachedDiscover::layer(discover, idle))
    }

    pub fn arc_new_http<T, B, Svc>(self) -> Stack<ArcNewHttp<T, B>>
    where
        T: 'static,
        B: 'static,
        S: NewService<T, Service = Svc> + Send + Sync + 'static,
        Svc: Service<http::Request<B>, Response = http::Response<http::BoxBody>, Error = Error>,
        Svc: Send + 'static,
        Svc::Future: Send,
    {
        self.arc_new_box()
    }

    pub fn arc_new_clone_http<T, B, Svc>(self) -> Stack<ArcNewCloneHttp<T, B>>
    where
        T: 'static,
        B: 'static,
        S: NewService<T, Service = Svc> + Send + Sync + 'static,
        Svc: Service<http::Request<B>, Response = http::Response<http::BoxBody>, Error = Error>,
        Svc: Clone + Send + 'static,
        Svc::Future: Send,
    {
        self.push_on_service(BoxCloneService::layer())
            .push(ArcNewService::layer())
    }

    pub fn arc_new_clone_sync_http<T, B, Svc>(self) -> Stack<ArcNewCloneSyncHttp<T, B>>
    where
        T: 'static,
        B: 'static,
        S: NewService<T, Service = Svc> + Send + Sync + 'static,
        Svc: Service<http::Request<B>, Response = http::Response<http::BoxBody>>,
        Svc: Clone + Send + Sync + 'static,
        Svc::Error: Into<Error>,
        Svc::Future: Send,
    {
        self.push_on_service(BoxCloneSyncService::layer())
            .push(ArcNewService::layer())
    }

    pub fn arc_new_tcp<T, I, Svc>(self) -> Stack<ArcNewTcp<T, I>>
    where
        T: 'static,
        I: 'static,
        S: NewService<T, Service = Svc> + Send + Sync + 'static,
        Svc: Service<I, Response = (), Error = Error>,
        Svc: Send + 'static,
        Svc::Future: Send,
    {
        self.arc_new_box()
    }

    pub fn arc_new_box<T, Req, Svc>(
        self,
    ) -> Stack<ArcNewService<T, BoxService<Req, Svc::Response, Error>>>
    where
        T: 'static,
        Req: 'static,
        S: NewService<T, Service = Svc> + Send + Sync + 'static,
        Svc: Service<Req, Error = Error>,
        Svc: Send + 'static,
        Svc::Future: Send,
    {
        self.push_on_service(BoxService::layer())
            .push(ArcNewService::layer())
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_new<T>(self) -> Self
    where
        S: NewService<T>,
    {
        self
    }

    pub fn check_new_new<T, U>(self) -> Self
    where
        S: NewService<T>,
        S::Service: NewService<U>,
    {
        self
    }

    pub fn check_new_new_service<T, U, Req>(self) -> Self
    where
        S: NewService<T>,
        S::Service: NewService<U>,
        <S::Service as NewService<U>>::Service: Service<Req>,
    {
        self
    }

    pub fn check_new_clone<T>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Clone,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_new_service<T, Req>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Service<Req>,
    {
        self
    }

    pub fn check_new_accept<T, I>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Service<I, Response = ()>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_clone_new_service<T, Req>(self) -> Self
    where
        S: NewService<T> + Clone,
        S::Service: Service<Req>,
    {
        self
    }

    /// Validates that this stack can be cloned
    pub fn check_clone(self) -> Self
    where
        S: Clone,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_service<T>(self) -> Self
    where
        S: Service<T>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets with `Unpin` futures.
    pub fn check_service_unpin<T>(self) -> Self
    where
        S: Service<T>,
        S::Future: Unpin,
    {
        self
    }

    pub fn check_service_response<T, U>(self) -> Self
    where
        S: Service<T, Response = U>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_make_service<T, U>(self) -> Self
    where
        S: MakeService<T, U>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_make_service_clone<T, U>(self) -> Self
    where
        S: MakeService<T, U> + Clone,
        S::Service: Clone,
    {
        self
    }

    pub fn check_new_send_and_static<M, T, Req>(self) -> Self
    where
        S: NewService<T, Service = M>,
        M: Service<Req> + Send + 'static,
        M::Response: Send + 'static,
        M::Error: Into<Error> + Send + Sync,
        M::Future: Send,
    {
        self
    }

    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<T, N> NewService<T> for Stack<N>
where
    N: NewService<T>,
{
    type Service = N::Service;

    fn new_service(&self, t: T) -> Self::Service {
        self.0.new_service(t)
    }
}

impl<T, S> Service<T> for Stack<S>
where
    S: Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, t: T) -> Self::Future {
        self.0.call(t)
    }
}

// === impl AlwaysReconnect ===

impl<E: Into<Error>> Recover<E> for AlwaysReconnect {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
