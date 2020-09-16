use linkerd2_app_core::Error;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio_test::io;

type ConnectFn = Box<dyn FnMut() -> ConnectFuture + Send>;

pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<io::Mock, Error>> + Send + 'static>>;

#[derive(Clone)]
pub struct Connect {
    endpoints: Arc<Mutex<HashMap<SocketAddr, ConnectFn>>>,
}

impl<E> tower::Service<E> for Connect
where
    E: Into<SocketAddr>,
{
    type Response = io::Mock;
    type Future = ConnectFuture;
    type Error = Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, endpoint: E) -> Self::Future {
        let addr = endpoint.into();
        tracing::trace!(%addr, "connect");
        match self.endpoints.lock().unwrap().get_mut(&addr) {
            Some(f) => (f)(),
            None => panic!("did not expect to connect to the endpoint {}", addr),
        }
    }
}

impl Connect {
    pub fn new() -> Self {
        Self {
            endpoints: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn endpoint_fn(
        self,
        endpoint: impl Into<SocketAddr>,
        f: impl (FnMut() -> ConnectFuture) + Send + 'static,
    ) -> Self {
        self.endpoints
            .lock()
            .unwrap()
            .insert(endpoint.into(), Box::new(f));
        self
    }

    pub fn endpoint_builder(
        self,
        endpoint: impl Into<SocketAddr>,
        mut builder: io::Builder,
    ) -> Self {
        self.endpoint_fn(endpoint, move || {
            let io = builder.build();
            Box::pin(async move { Ok(io) })
        })
    }
}
