// Possibly unused, but useful during development.

pub use crate::proxy::http;
use crate::transport::Connect;
use crate::{cache, Error};
pub use linkerd2_buffer as buffer;
use linkerd2_concurrency_limit as concurrency_limit;
pub use linkerd2_stack::{self as stack, layer, NewService};
pub use linkerd2_stack_tracing::{InstrumentMake, InstrumentMakeLayer};
pub use linkerd2_timeout as timeout;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::layer::util::{Identity, Stack as Pair};
pub use tower::layer::Layer;
pub use tower::make::MakeService;
pub use tower::spawn_ready::SpawnReady;
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, Service, ServiceExt};

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

pub fn connect(keepalive: Option<Duration>) -> Stack<Connect> {
    Stack(Connect::new(keepalive))
}

pub fn proxies() -> Stack<IdentityProxy> {
    Stack(IdentityProxy(()))
}

#[derive(Copy, Clone, Debug)]
pub struct IdentityProxy(());

impl<T> NewService<T> for IdentityProxy {
    type Service = ();
    fn new_service(&mut self, _: T) -> Self::Service {
        ()
    }
}

#[allow(dead_code)]
impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    pub fn push_map_target<M>(self, map_target: M) -> Layers<Pair<L, stack::MapTargetLayer<M>>> {
        self.push(stack::MapTargetLayer::new(map_target))
    }

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn push_into_new_service(
        self,
    ) -> Layers<Pair<L, stack::new_service::FromMakeServiceLayer>> {
        self.push(stack::new_service::FromMakeServiceLayer::default())
    }

    /// Buffers requests in an mpsc, spawning the inner service onto a dedicated task.
    pub fn push_spawn_buffer<Req, Rsp>(
        self,
        capacity: usize,
    ) -> Layers<Pair<L, buffer::SpawnBufferLayer<Req, Rsp>>>
    where
        Req: Send + 'static,
        Rsp: Send + 'static,
    {
        self.push(buffer::SpawnBufferLayer::new(capacity))
    }

    pub fn push_spawn_buffer_with_idle_timeout<Req, Rsp>(
        self,
        capacity: usize,
        idle_timeout: Duration,
    ) -> Layers<Pair<L, buffer::SpawnBufferLayer<Req, Rsp>>>
    where
        Req: Send + 'static,
        Rsp: Send + 'static,
    {
        self.push(buffer::SpawnBufferLayer::new(capacity).with_idle_timeout(idle_timeout))
    }

    // Makes the service eagerly process and fail requests after the given timeout.
    pub fn push_failfast(self, timeout: Duration) -> Layers<Pair<L, timeout::FailFastLayer>> {
        self.push(timeout::FailFastLayer::new(timeout))
    }

    pub fn push_on_response<U>(self, layer: U) -> Layers<Pair<L, stack::OnResponseLayer<U>>> {
        self.push(stack::OnResponseLayer::new(layer))
    }

    pub fn push_concurrency_limit(self, max: usize) -> Layers<Pair<L, concurrency_limit::Layer>> {
        self.push(concurrency_limit::Layer::new(max))
    }

    pub fn push_make_ready<Req>(self) -> Layers<Pair<L, stack::MakeReadyLayer<Req>>> {
        self.push(stack::MakeReadyLayer::new())
    }

    pub fn push_map_response<R: Clone>(
        self,
        map_response: R,
    ) -> Layers<Pair<L, stack::MapResponseLayer<R>>> {
        self.push(stack::MapResponseLayer::new(map_response))
    }

    pub fn box_http_request<B>(self) -> Layers<Pair<L, http::boxed::request::Layer<B>>>
    where
        B: hyper::body::HttpBody + 'static,
    {
        self.push(http::boxed::request::Layer::new())
    }

    pub fn box_http_response(self) -> Layers<Pair<L, http::boxed::response::Layer>> {
        self.push(http::boxed::response::Layer::new())
    }

    pub fn push_oneshot(self) -> Layers<Pair<L, stack::OneshotLayer>> {
        self.push(stack::OneshotLayer::new())
    }

    pub fn push_instrument<G: Clone>(self, get_span: G) -> Layers<Pair<L, InstrumentMakeLayer<G>>> {
        self.push(InstrumentMakeLayer::new(get_span))
    }
}

impl<M, L: Layer<M>> Layer<M> for Layers<L> {
    type Service = L::Service;

    fn layer(&self, inner: M) -> Self::Service {
        self.0.layer(inner)
    }
}

#[allow(dead_code)]
impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    pub fn push_map_target<M: Clone>(
        self,
        map_target: M,
    ) -> Stack<stack::map_target::MapTargetService<S, M>> {
        self.push(stack::map_target::MapTargetLayer::new(map_target))
    }

    pub fn push_request_filter<F: Clone>(self, filter: F) -> Stack<stack::RequestFilter<F, S>> {
        self.push(layer::mk(|inner| {
            stack::RequestFilter::new(filter.clone(), inner)
        }))
    }

    /// Wraps a `Service<T>` as a `Service<()>`.
    ///
    /// Each time the service is called, the `T`-typed request is cloned and
    /// issued into the inner service.
    pub fn push_make_thunk(self) -> Stack<stack::make_thunk::MakeThunk<S>> {
        self.push(layer::mk(stack::make_thunk::MakeThunk::new))
    }

    pub fn instrument<G: Clone>(self, get_span: G) -> Stack<InstrumentMake<G, S>> {
        self.push(InstrumentMakeLayer::new(get_span))
    }

    pub fn instrument_from_target(self) -> Stack<InstrumentMake<(), S>> {
        self.push(InstrumentMakeLayer::from_target())
    }

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn into_new_service(self) -> Stack<stack::new_service::FromMakeService<S>> {
        self.push(stack::new_service::FromMakeServiceLayer::default())
    }

    pub fn into_make_service<T>(self) -> Stack<stack::new_service::IntoMakeService<S>>
    where
        S: NewService<T>,
    {
        Stack(stack::new_service::IntoMakeService::new(self.0))
    }

    pub fn push_make_ready<Req>(self) -> Stack<stack::MakeReady<S, Req>> {
        self.push(stack::MakeReadyLayer::new())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn spawn_buffer<Req, Rsp>(self, capacity: usize) -> Stack<buffer::Buffer<Req, Rsp>>
    where
        Req: Send + 'static,
        Rsp: Send + 'static,
        S: Service<Req, Response = Rsp> + Send + 'static,
        S::Error: Into<Error> + Send + Sync,
        S::Future: Send,
    {
        self.push(buffer::SpawnBufferLayer::new(capacity))
    }

    /// Assuming `S` implements `NewService` or `MakeService`, applies the given
    /// `L`-typed layer on each service produced by `S`.
    pub fn push_on_response<L: Clone>(self, layer: L) -> Stack<stack::OnResponse<L, S>> {
        self.push(stack::OnResponseLayer::new(layer))
    }

    pub fn push_concurrency_limit(
        self,
        max: usize,
    ) -> Stack<concurrency_limit::ConcurrencyLimit<S>> {
        self.push(concurrency_limit::Layer::new(max))
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(tower::timeout::TimeoutLayer::new(timeout))
    }

    // Makes the service eagerly process and fail requests after the given timeout.
    pub fn push_failfast(self, timeout: Duration) -> Stack<timeout::FailFast<S>> {
        self.push(timeout::FailFastLayer::new(timeout))
    }

    pub fn push_oneshot(self) -> Stack<stack::Oneshot<S>> {
        self.push(stack::OneshotLayer::new())
    }

    pub fn push_map_response<R: Clone>(self, map_response: R) -> Stack<stack::MapResponse<S, R>> {
        self.push(stack::MapResponseLayer::new(map_response))
    }

    pub fn push_http_insert_target(self) -> Stack<http::insert::target::NewService<S>> {
        self.push(http::insert::target::layer())
    }

    pub fn cache<T, L, U>(self, track: L) -> Stack<cache::Cache<T, cache::layer::NewTrack<L, S>>>
    where
        T: Eq + std::hash::Hash + Send + 'static,
        S: NewService<T> + Clone,
        L: tower::layer::Layer<cache::layer::Track<S>> + Clone,
        L::Service: NewService<T, Service = U>,
    {
        self.push(cache::CacheLayer::new(track))
    }

    /// Push a service that either calls the inner service if it is ready, or
    /// calls a `secondary` service if the inner service fails to become ready
    /// for the `skip_after` duration.
    pub fn push_when_unready<B: Clone>(
        self,
        secondary: B,
        skip_after: Duration,
    ) -> Stack<stack::NewSwitchReady<S, B>> {
        self.push(layer::mk(|inner: S| {
            stack::NewSwitchReady::new(inner, secondary.clone(), skip_after)
        }))
    }

    pub fn push_switch<T: Clone, U: Clone>(
        self,
        switch: T,
        other: U,
    ) -> Stack<stack::MakeSwitch<T, S, U>> {
        self.push(layer::mk(|inner: S| {
            stack::MakeSwitch::new(switch.clone(), inner, other.clone())
        }))
    }

    // pub fn box_http_request<B>(self) -> Stack<http::boxed::BoxRequest<S, B>>
    // where
    //     B: hyper::body::HttpBody<Data = http::boxed::Data, Error = Error> + 'static,
    //     S: tower::Service<http::Request<http::boxed::BoxBody>>,
    // {
    //     self.push(http::boxed::request::Layer::new())
    // }

    pub fn box_http_response(self) -> Stack<http::boxed::BoxResponse<S>> {
        self.push(http::boxed::response::Layer::new())
    }

    pub fn box_new_service<T>(self) -> Stack<stack::BoxNewService<T, S::Service>>
    where
        S: NewService<T> + Clone + Send + Sync + 'static,
        S::Service: Send + 'static,
    {
        self.push(layer::mk(stack::BoxNewService::new))
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_new<T>(self) -> Self
    where
        S: NewService<T>,
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

    fn new_service(&mut self, t: T) -> Self::Service {
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

pub mod make_response {
    use super::Oneshot;
    use crate::Error;
    use futures::TryFuture;
    use pin_project::pin_project;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    #[derive(Copy, Clone, Debug)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct MakeResponse<M>(M);

    #[pin_project]
    pub struct ResponseFuture<F, S: tower::Service<()>> {
        #[pin]
        state: State<F, S>,
    }

    #[pin_project(project = StateProj)]
    enum State<F, S: tower::Service<()>> {
        Make(#[pin] F),
        Respond(#[pin] Oneshot<S, ()>),
    }

    impl<S> super::Layer<S> for Layer {
        type Service = MakeResponse<S>;

        fn layer(&self, inner: S) -> Self::Service {
            MakeResponse(inner)
        }
    }

    impl<T, M> tower::Service<T> for MakeResponse<M>
    where
        M: tower::make::MakeService<T, ()>,
        M::MakeError: Into<Error>,
        M::Error: Into<Error>,
    {
        type Response = M::Response;
        type Error = Error;
        type Future = ResponseFuture<M::Future, M::Service>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx).map_err(Into::into)
        }

        fn call(&mut self, req: T) -> Self::Future {
            ResponseFuture {
                state: State::Make(self.0.make_service(req)),
            }
        }
    }

    impl<F, S> Future for ResponseFuture<F, S>
    where
        F: TryFuture<Ok = S>,
        F::Error: Into<Error>,
        S: tower::Service<()>,
        S::Error: Into<Error>,
    {
        type Output = Result<S::Response, Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut this = self.project();
            loop {
                match this.state.as_mut().project() {
                    StateProj::Make(fut) => {
                        let svc = futures::ready!(fut.try_poll(cx)).map_err(Into::into)?;
                        this.state.set(State::Respond(Oneshot::new(svc, ())))
                    }
                    StateProj::Respond(fut) => return fut.poll(cx).map_err(Into::into),
                }
            }
        }
    }
}
