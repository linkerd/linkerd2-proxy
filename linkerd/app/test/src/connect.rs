use linkerd_app_core::{io::BoxedIo, svc::Param, transport::ConnectAddr, Error};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio_test::io;
use tracing::instrument::{Instrument, Instrumented};

type ConnectFn<E> = Box<dyn FnMut(E) -> ConnectFuture + Send>;

pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<BoxedIo, Error>> + Send + 'static>>;

#[derive(Clone)]
pub struct Connect<E> {
    endpoints: Arc<Mutex<HashMap<SocketAddr, ConnectFn<E>>>>,
}

#[derive(Clone)]
pub struct NoRawTcp;

impl<E> tower::Service<E> for Connect<E>
where
    E: Clone + fmt::Debug + Param<ConnectAddr>,
{
    type Response = BoxedIo;
    type Future = Instrumented<ConnectFuture>;
    type Error = Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, endpoint: E) -> Self::Future {
        let ConnectAddr(addr) = endpoint.param();
        let span = tracing::info_span!("connect", %addr);
        let f = span.in_scope(|| {
            tracing::trace!("connecting...");
            let mut endpoints = self.endpoints.lock().unwrap();
            match endpoints.get_mut(&addr) {
                Some(f) => (f)(endpoint),
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
    type Response = BoxedIo;
    type Future = Instrumented<ConnectFuture>;
    type Error = Error;

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
        mut on_connect: impl (FnMut(E) -> Result<BoxedIo, Error>) + Send + 'static,
    ) -> Self {
        self.endpoints.lock().unwrap().insert(
            endpoint.into(),
            Box::new(move |endpoint| {
                let conn = on_connect(endpoint);
                Box::pin(async move { conn })
            }),
        );
        self
    }

    pub fn endpoint_fn(
        self,
        endpoint: impl Into<SocketAddr>,
        mut on_connect: impl (FnMut(E) -> Result<io::Mock, Error>) + Send + 'static,
    ) -> Self {
        self.endpoint_fn_boxed(endpoint, move |ep| on_connect(ep).map(BoxedIo::new))
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
