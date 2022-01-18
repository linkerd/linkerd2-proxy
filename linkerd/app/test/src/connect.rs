use linkerd_app_core::{
    svc::{Param, Service},
    transport::{ClientAddr, Local, Remote, ServerAddr},
};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tracing::instrument::{Instrument, Instrumented};

mod io {
    pub use linkerd_app_core::io::*;
    pub use tokio_test::io::*;
}

type ConnectFn<T> = Box<dyn FnMut(T) -> ConnectFuture + Send>;

pub type ConnectFuture =
    Pin<Box<dyn Future<Output = io::Result<(io::BoxedIo, Local<ClientAddr>)>> + Send + 'static>>;

#[derive(Clone)]
pub struct Connect<E> {
    endpoints: Arc<Mutex<HashMap<SocketAddr, ConnectFn<E>>>>,
}

#[derive(Clone)]
pub struct NoRawTcp;

impl<T> Service<T> for Connect<T>
where
    T: Clone + fmt::Debug + Param<Remote<ServerAddr>>,
{
    type Response = (io::BoxedIo, Local<ClientAddr>);
    type Future = Instrumented<ConnectFuture>;
    type Error = io::Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let Remote(ServerAddr(addr)) = target.param();
        let span = tracing::info_span!("connect", %addr);
        let f = span.in_scope(|| {
            tracing::trace!("connecting...");
            let mut endpoints = self.endpoints.lock().unwrap();
            match endpoints.get_mut(&addr) {
                Some(f) => (f)(target),
                None => panic!(
                    "did not expect to connect to the endpoint {} not in {:?}",
                    addr,
                    endpoints.keys().collect::<Vec<_>>()
                ),
            }
        });
        f.instrument(span)
    }
}

impl<E: fmt::Debug> tower::Service<E> for NoRawTcp {
    type Response = (io::BoxedIo, Local<ClientAddr>);
    type Future = Instrumented<ConnectFuture>;
    type Error = io::Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        panic!("no raw TCP connections expected in this test");
    }

    fn call(&mut self, endpoint: E) -> Self::Future {
        panic!(
            "no raw TCP connections expected in this test, but tried to connect to {:?}",
            endpoint
        );
    }
}

impl<E> Default for Connect<E> {
    fn default() -> Self {
        Self {
            endpoints: Default::default(),
        }
    }
}

impl<E: fmt::Debug> Connect<E> {
    pub fn endpoint(
        self,
        endpoint: impl Into<SocketAddr>,
        on_connect: impl Into<Box<dyn FnMut(E) -> ConnectFuture + Send + 'static>>,
    ) -> Self {
        self.endpoints
            .lock()
            .unwrap()
            .insert(endpoint.into(), on_connect.into());
        self
    }

    pub fn endpoint_fn_boxed(
        self,
        endpoint: impl Into<SocketAddr>,
        mut on_connect: impl (FnMut(E) -> io::Result<io::BoxedIo>) + Send + 'static,
    ) -> Self {
        self.endpoints.lock().unwrap().insert(
            endpoint.into(),
            Box::new(move |endpoint| {
                let conn = on_connect(endpoint);
                let local = Local(ClientAddr(([0, 0, 0, 0], 0).into()));
                Box::pin(async move { conn.map(move |c| (c, local)) })
            }),
        );
        self
    }

    pub fn endpoint_fn(
        self,
        endpoint: impl Into<SocketAddr>,
        mut on_connect: impl (FnMut(E) -> io::Result<io::Mock>) + Send + 'static,
    ) -> Self {
        self.endpoint_fn_boxed(endpoint, move |ep| on_connect(ep).map(io::BoxedIo::new))
    }

    pub fn endpoint_builder(
        self,
        endpoint: impl Into<SocketAddr>,
        mut builder: io::Builder,
    ) -> Self {
        self.endpoint_fn(endpoint, move |endpoint| {
            tracing::trace!(?endpoint, "building mock IO for");
            let io = builder.build();
            Ok(io)
        })
    }
}
