use linkerd2_addr::Addr;
use std::fmt;

#[derive(Clone, Debug)]
pub struct ControlAddr {
    pub addr: Addr,
    pub identity: crate::transport::tls::PeerIdentity,
}

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

/// Sets the request's URI from `Config`.
pub mod add_origin {
    use super::ControlAddr;
    use futures::{ready, TryFuture};
    use linkerd2_error::Error;
    use pin_project::pin_project;
    use std::future::Future;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tower_request_modifier::{Builder, RequestModifier};

    #[derive(Debug)]
    pub struct Layer<B> {
        _marker: PhantomData<fn(B)>,
    }

    #[derive(Debug)]
    pub struct MakeAddOrigin<M, B> {
        inner: M,
        _marker: PhantomData<fn(B)>,
    }

    #[pin_project]
    pub struct MakeFuture<F, B> {
        #[pin]
        inner: F,
        authority: http::uri::Authority,
        _marker: PhantomData<fn(B)>,
    }

    // === impl Layer ===

    impl<B> Layer<B> {
        pub fn new() -> Self {
            Layer {
                _marker: PhantomData,
            }
        }
    }

    impl<B> Clone for Layer<B> {
        fn clone(&self) -> Self {
            Self {
                _marker: self._marker,
            }
        }
    }

    impl<M, B> tower::layer::Layer<M> for Layer<B> {
        type Service = MakeAddOrigin<M, B>;

        fn layer(&self, inner: M) -> Self::Service {
            Self::Service {
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeAddOrigin ===

    impl<M, B> tower::Service<ControlAddr> for MakeAddOrigin<M, B>
    where
        M: tower::Service<ControlAddr>,
        M::Error: Into<Error>,
    {
        type Response = RequestModifier<M::Response, B>;
        type Error = Error;
        type Future = MakeFuture<M::Future, B>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx).map_err(Into::into)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let authority = target.addr.to_http_authority();
            let inner = self.inner.call(target);
            MakeFuture {
                inner,
                authority,
                _marker: PhantomData,
            }
        }
    }

    impl<M, B> Clone for MakeAddOrigin<M, B>
    where
        M: tower::Service<ControlAddr> + Clone,
    {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeFuture ===

    impl<F, B> Future for MakeFuture<F, B>
    where
        F: TryFuture,
        F::Error: Into<Error>,
    {
        type Output = Result<RequestModifier<F::Ok, B>, Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let inner = ready!(this.inner.try_poll(cx).map_err(Into::into))?;

            Poll::Ready(
                Builder::new()
                    .set_origin(format!("http://{}", this.authority))
                    .build(inner)
                    .map_err(|_| BuildError.into()),
            )
        }
    }

    // XXX the request_modifier build error does not implement Error...
    #[derive(Debug)]
    struct BuildError;

    impl std::error::Error for BuildError {}
    impl std::fmt::Display for BuildError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "failed to build the add-origin request modifier")
        }
    }
}

/// Resolves the controller's `addr` once before building a client.
pub mod resolve {
    use super::{client, ControlAddr};
    use crate::svc;
    use futures::{ready, TryFuture};
    use linkerd2_addr::Addr;
    use linkerd2_dns as dns;
    use pin_project::pin_project;
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::{error, fmt};

    #[derive(Clone, Debug)]
    pub struct Layer {
        dns: dns::Resolver,
    }

    #[derive(Clone, Debug)]
    pub struct Resolve<M> {
        dns: dns::Resolver,
        inner: M,
    }

    #[pin_project]
    pub struct Init<M>
    where
        M: tower::Service<client::Target>,
    {
        #[pin]
        state: State<M>,
    }

    #[pin_project(project = StateProj)]
    enum State<M>
    where
        M: tower::Service<client::Target>,
    {
        Resolve(#[pin] dns::IpAddrFuture, Option<(M, ControlAddr)>),
        NotReady(M, Option<(SocketAddr, ControlAddr)>),
        Inner(#[pin] M::Future),
    }

    #[derive(Debug)]
    pub enum Error<I> {
        Dns(dns::Error),
        Inner(I),
    }

    // === impl Layer ===

    pub fn layer<M>(dns: dns::Resolver) -> impl svc::Layer<M, Service = Resolve<M>> + Clone
    where
        M: tower::Service<client::Target> + Clone,
    {
        svc::layer::mk(move |inner| Resolve {
            dns: dns.clone(),
            inner,
        })
    }

    // === impl Resolve ===

    impl<M> tower::Service<ControlAddr> for Resolve<M>
    where
        M: tower::Service<client::Target> + Clone,
    {
        type Response = M::Response;
        type Error = <Init<M> as TryFuture>::Error;
        type Future = Init<M>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx).map_err(Error::Inner)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let state = match target.addr {
                Addr::Socket(sa) => State::make_inner(sa, &target, &mut self.inner),
                Addr::Name(ref na) => {
                    // The inner service is ready, but we are going to do
                    // additional work before using it. In case the inner
                    // service has acquired resources (like a lock), we
                    // relinquish our claim on the service by replacing it.
                    self.inner = self.inner.clone();

                    let future = self.dns.resolve_one_ip(na.name());
                    State::Resolve(future, Some((self.inner.clone(), target.clone())))
                }
            };

            Init { state }
        }
    }

    // === impl Init ===

    impl<M> Future for Init<M>
    where
        M: tower::Service<client::Target>,
    {
        type Output = Result<M::Response, Error<M::Error>>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut this = self.project();
            loop {
                match this.state.as_mut().project() {
                    StateProj::Resolve(fut, stack) => {
                        let ip = ready!(fut.poll(cx).map_err(Error::Dns))?;
                        let (svc, config) = stack.take().unwrap();
                        let addr = SocketAddr::from((ip, config.addr.port()));
                        this.state
                            .as_mut()
                            .set(State::NotReady(svc, Some((addr, config))));
                    }
                    StateProj::NotReady(svc, cfg) => {
                        ready!(svc.poll_ready(cx).map_err(Error::Inner))?;
                        let (addr, config) = cfg.take().unwrap();
                        let state = State::make_inner(addr, &config, svc);
                        this.state.as_mut().set(state);
                    }
                    StateProj::Inner(fut) => return fut.poll(cx).map_err(Error::Inner),
                };
            }
        }
    }

    impl<M> State<M>
    where
        M: tower::Service<client::Target>,
    {
        fn make_inner(addr: SocketAddr, dst: &ControlAddr, mk_svc: &mut M) -> Self {
            let target = client::Target {
                addr,
                server_name: dst.identity.clone(),
            };

            State::Inner(mk_svc.call(target))
        }
    }

    // === impl Error ===

    impl<I: fmt::Display> fmt::Display for Error<I> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Error::Dns(dns::Error::NoAddressesFound(_)) => write!(f, "no addresses found"),
                Error::Dns(e) => fmt::Display::fmt(&e, f),
                Error::Inner(ref e) => fmt::Display::fmt(&e, f),
            }
        }
    }

    impl<I: fmt::Debug + fmt::Display> error::Error for Error<I> {}
}

pub mod dns_resolve {
    use super::{client::Target, ControlAddr};
    use futures::ready;
    use linkerd2_addr::Addr;
    use linkerd2_dns as dns;
    use linkerd2_error::Error;
    use linkerd2_proxy_core::resolve::{self, Update};

    use pin_project::pin_project;
    use std::collections::{HashSet, VecDeque};
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::time;
    use tracing::debug;

    #[derive(Clone)]
    pub struct Resolve {
        dns: dns::Resolver,
    }

    #[pin_project]
    pub struct Resolution {
        current_targets: HashSet<Target>,
        address: ControlAddr,
        dns: dns::Resolver,
        #[pin]
        state: State,
    }

    #[pin_project(project = StateProj)]
    enum State {
        Resolving(#[pin] dns::ResolveResponseFuture),
        Resolved(VecDeque<Update<Target>>, #[pin] time::Delay),
        Constant(Option<Target>),
    }

    // === impl ControlPlaneResolve ===

    impl Resolve {
        pub fn new(dns: dns::Resolver) -> Self {
            Self { dns }
        }
    }

    impl tower::Service<ControlAddr> for Resolve {
        type Response = Resolution;
        type Error = Error;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            Box::pin(futures::future::ok(match target.addr {
                Addr::Socket(sa) => Resolution {
                    current_targets: HashSet::default(),
                    address: target.clone(),
                    dns: self.dns.clone(),
                    state: State::Constant(Some(Target {
                        addr: sa,
                        server_name: target.identity.clone(),
                    })),
                },
                Addr::Name(ref na) => Resolution {
                    current_targets: HashSet::default(),
                    address: target.clone(),
                    dns: self.dns.clone(),
                    state: State::Resolving(self.dns.resolve_ips(na.name())),
                },
            }))
        }
    }

    impl Resolution {
        pub fn diff_targets(
            old_targets: &HashSet<Target>,
            new_targets: &HashSet<Target>,
        ) -> (Vec<(SocketAddr, Target)>, Vec<SocketAddr>) {
            let mut adds = Vec::default();
            let mut removes = Vec::default();

            for new_target in new_targets.iter() {
                if !old_targets.contains(new_target) {
                    adds.push((new_target.addr, new_target.clone()))
                }
            }

            for old_target in old_targets.iter() {
                if !new_targets.contains(old_target) {
                    removes.push(old_target.addr)
                }
            }

            (adds, removes)
        }
    }

    // === impl ControlPlaneResolution ===

    impl resolve::Resolution for Resolution {
        type Endpoint = Target;
        type Error = Error;

        fn poll(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<Update<Self::Endpoint>, Self::Error>> {
            let mut this = self.project();
            loop {
                match this.state.as_mut().project() {
                    StateProj::Constant(address) => {
                        if let Some(target) = address.take() {
                            return Poll::Ready(Ok(Update::Add(vec![(target.addr, target)])));
                        }
                        return Poll::Pending;
                    }
                    StateProj::Resolving(f) => {
                        debug!("resolving");

                        let new_state = match ready!(f.poll(cx)) {
                            Err(dns::Error::NoAddressesFound(valid_until)) => {
                                debug!("resolved empty");
                                this.current_targets.clear();
                                State::Resolved(
                                    vec![Update::Empty].into_iter().collect(),
                                    time::delay_until(valid_until),
                                )
                            }
                            Err(dns::Error::ResolutionFailed(inner)) => {
                                if let dns::ResolveErrorKind::NoRecordsFound {
                                    valid_until: Some(valid_until),
                                    ..
                                } = inner.kind()
                                {
                                    debug!("resolved does not exist");
                                    this.current_targets.clear();
                                    State::Resolved(
                                        vec![Update::DoesNotExist].into_iter().collect(),
                                        time::delay_until(time::Instant::from_std(
                                            valid_until.clone(),
                                        )),
                                    )
                                } else {
                                    return Poll::Ready(Err(
                                        dns::Error::ResolutionFailed(inner).into()
                                    ));
                                }
                            }
                            Err(err) => return Poll::Ready(Err(err.into())),
                            Ok(result) => {
                                let new_targets = result
                                    .ips
                                    .into_iter()
                                    .map(|ip| Target {
                                        addr: SocketAddr::from((ip, this.address.addr.port())),
                                        server_name: this.address.identity.clone(),
                                    })
                                    .collect();

                                let (adds, removes) =
                                    Resolution::diff_targets(this.current_targets, &new_targets);
                                let mut new_updates = VecDeque::default();
                                if !adds.is_empty() {
                                    new_updates.push_back(Update::Add(adds))
                                }
                                if !removes.is_empty() {
                                    new_updates.push_back(Update::Remove(removes))
                                }

                                this.current_targets.clear();
                                this.current_targets.extend(new_targets);

                                debug!(?new_updates, "resolved");
                                State::Resolved(new_updates, time::delay_until(result.valid_until))
                            }
                        };

                        this.state.as_mut().set(new_state);
                    }
                    StateProj::Resolved(new_updates, delay) => {
                        if let Some(update) = new_updates.pop_front() {
                            debug!(?update, "emitting");
                            return Poll::Ready(Ok(update));
                        }
                        ready!(delay.poll(cx));
                        let name = this
                            .address
                            .addr
                            .name_addr()
                            .expect("should be name addr if resolving")
                            .name();
                        this.state
                            .as_mut()
                            .set(State::Resolving(this.dns.resolve_ips(name)));
                    }
                };
            }
        }
    }
}

/// Creates a client suitable for gRPC.
pub mod client {
    use crate::transport::{connect, tls};
    use crate::{proxy::http, svc};
    use linkerd2_proxy_http::h2::Settings as H2Settings;
    use std::net::SocketAddr;
    use std::task::{Context, Poll};

    #[derive(Clone, Hash, Debug, Eq, PartialEq)]
    pub struct Target {
        pub(super) addr: SocketAddr,
        pub(super) server_name: tls::PeerIdentity,
    }

    #[derive(Debug)]
    pub struct Client<C, B> {
        inner: http::h2::Connect<C, B>,
    }

    // === impl Target ===

    impl connect::ConnectAddr for Target {
        fn connect_addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl tls::HasPeerIdentity for Target {
        fn peer_identity(&self) -> tls::PeerIdentity {
            self.server_name.clone()
        }
    }

    // === impl Layer ===

    pub fn layer<C, B>() -> impl svc::Layer<C, Service = Client<C, B>> + Copy
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        svc::layer::mk(|mk_conn| {
            let inner = http::h2::Connect::new(mk_conn, H2Settings::default());
            Client { inner }
        })
    }

    // === impl Client ===

    impl<C, B> tower::Service<Target> for Client<C, B>
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        type Response = <http::h2::Connect<C, B> as tower::Service<Target>>::Response;
        type Error = <http::h2::Connect<C, B> as tower::Service<Target>>::Error;
        type Future = <http::h2::Connect<C, B> as tower::Service<Target>>::Future;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        #[inline]
        fn call(&mut self, target: Target) -> Self::Future {
            self.inner.call(target)
        }
    }

    // A manual impl is needed since derive adds `B: Clone`, but that's just
    // a PhantomData.
    impl<C, B> Clone for Client<C, B>
    where
        http::h2::Connect<C, B>: Clone,
    {
        fn clone(&self) -> Self {
            Client {
                inner: self.inner.clone(),
            }
        }
    }
}
