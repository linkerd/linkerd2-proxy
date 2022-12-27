// Possibly unused, but useful during development.

pub use crate::proxy::http;
use crate::{cache, config::BufferConfig, Error};
use linkerd_error::Recover;
use linkerd_exp_backoff::{ExponentialBackoff, ExponentialBackoffStream};
pub use linkerd_reconnect::NewReconnect;
pub use linkerd_stack::{
    self as stack, layer, ArcNewService, BoxCloneService, BoxService, BoxServiceLayer, Either,
    ExtractParam, Fail, FailFast, Filter, InsertParam, MakeConnection, MapErr, MapTargetLayer,
    NewCloneService, NewRouter, NewService, Oneshot, Param, Predicate, UnwrapOr,
};
pub use linkerd_stack_tracing::{GetSpan, NewInstrument, NewInstrumentLayer};
use stack::OnService;
use std::{
    marker::PhantomData,
    task::{Context, Poll},
    time::Duration,
};
use tower::{
    buffer::Buffer as TowerBuffer,
    layer::util::{Identity, Stack as Pair},
    make::MakeService,
};
pub use tower::{
    layer::Layer, limit::GlobalConcurrencyLimitLayer as ConcurrencyLimitLayer, service_fn as mk,
    spawn_ready::SpawnReady, Service, ServiceExt,
};

#[derive(Copy, Clone, Debug)]
pub struct AlwaysReconnect(ExponentialBackoff);

pub type Buffer<Req, Rsp, E> = TowerBuffer<BoxService<Req, Rsp, E>, Req>;

pub struct BufferLayer<Req> {
    name: &'static str,
    capacity: usize,
    failfast_timeout: Duration,
    _marker: PhantomData<fn(Req)>,
}

pub type BoxHttp<B = http::BoxBody> =
    BoxService<http::Request<B>, http::Response<http::BoxBody>, Error>;

pub type BoxCloneHttp<B = http::BoxBody> =
    BoxCloneService<http::Request<B>, http::Response<http::BoxBody>, Error>;

pub type ArcNewHttp<T, B = http::BoxBody> = ArcNewService<T, BoxHttp<B>>;

pub type ArcNewCloneHttp<T, B = http::BoxBody> = ArcNewService<T, BoxCloneHttp<B>>;

pub type BoxTcp<I> = BoxService<I, (), Error>;

pub type ArcNewTcp<T, I> = ArcNewService<T, BoxTcp<I>>;

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

    /// Buffers requests in an mpsc, spawning the inner service onto a dedicated task.
    pub fn push_buffer<Req>(
        self,
        name: &'static str,
        config: &BufferConfig,
    ) -> Layers<Pair<L, BufferLayer<Req>>>
    where
        Req: Send + 'static,
    {
        self.push(buffer(name, config))
    }

    pub fn push_on_service<U>(self, layer: U) -> Layers<Pair<L, stack::OnServiceLayer<U>>> {
        self.push(stack::OnServiceLayer::new(layer))
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

    pub fn push_request_filter<F: Clone>(self, filter: F) -> Stack<stack::Filter<S, F>> {
        self.push(stack::Filter::<S, F>::layer(filter))
    }

    /// Wraps a `Service<T>` as a `Service<()>`.
    ///
    /// Each time the service is called, the `T`-typed request is cloned and
    /// issued into the inner service.
    pub fn push_make_thunk(self) -> Stack<stack::MakeThunk<S>> {
        self.push(layer::mk(stack::MakeThunk::new))
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

    /// Wraps the inner service with a response timeout such that timeout errors are surfaced as a
    /// `ConnectTimeout` error.
    ///
    /// Note that any timeouts errors from the inner service will be wrapped as well.
    pub fn push_connect_timeout(
        self,
        timeout: Duration,
    ) -> Stack<MapErr<stack::Timeout<S>, impl FnOnce(Error) -> Error + Clone>> {
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

    pub fn push_buffer_on_service<Req>(
        self,
        name: &'static str,
        config: &BufferConfig,
    ) -> Stack<OnService<BufferLayer<Req>, S>>
    where
        Req: Send + 'static,
    {
        self.push_on_service(buffer(name, config))
    }

    pub fn push_cache<T>(self, idle: Duration) -> Stack<cache::NewCachedService<T, S>>
    where
        T: Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
        S: NewService<T> + 'static,
        S::Service: Send + Sync + 'static,
    {
        self.push(cache::NewCachedService::layer(idle))
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

// === impl BufferLayer ===

fn buffer<Req>(name: &'static str, config: &BufferConfig) -> BufferLayer<Req> {
    BufferLayer {
        name,
        capacity: config.capacity,
        failfast_timeout: config.failfast_timeout,
        _marker: PhantomData,
    }
}

impl<Req, S> Layer<S> for BufferLayer<Req>
where
    Req: Send + 'static,
    S: Service<Req, Error = Error> + Send + 'static,
    S::Future: Send,
{
    type Service = Buffer<Req, S::Response, Error>;

    fn layer(&self, inner: S) -> Self::Service {
        let spawn_ready = SpawnReady::new(inner);
        let fail_fast = FailFast::layer(self.name, self.failfast_timeout).layer(spawn_ready);
        Buffer::new(BoxService::new(fail_fast), self.capacity)
    }
}

impl<Req> Clone for BufferLayer<Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            name: self.name,
            failfast_timeout: self.failfast_timeout,
            _marker: self._marker,
        }
    }
}
