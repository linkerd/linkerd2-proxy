use crate::app_core::{
    transport::{
        io::BoxedIo,
        tls::{HasPeerIdentity, PeerIdentity, ReasonForNoPeerName},
    },
    Conditional, Error,
};
use hyper::{Body, Request, Response};
use linkerd2_identity::Name;
use std::sync::{Arc, Mutex};
use tracing::Instrument;

pub struct Server {
    settings: hyper::server::conn::Http,
    identity: Option<PeerIdentity>,
    f: Box<dyn (FnMut(Request<Body>) -> Result<Response<Body>, Error>) + Send>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            settings: hyper::server::conn::Http::new(),
            identity: None,
            f: Box::new(|_| {
                Ok(Response::builder()
                    .status(http::status::StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .expect("known status code is fine"))
            }),
        }
    }
}

impl Server {
    pub fn http1(mut self) -> Self {
        self.settings.http1_only(true);
        self
    }

    pub fn http2(mut self) -> Self {
        self.settings.http2_only(true);
        self
    }

    pub fn expect_identity(mut self, id: impl Into<Name>) -> Self {
        self.identity = Some(Conditional::Some(id.into()));
        self
    }

    pub fn no_identity(mut self, reason: ReasonForNoPeerName) -> Self {
        self.identity = Some(Conditional::None(reason));
        self
    }

    pub fn new(mut f: impl (FnMut(Request<Body>) -> Response<Body>) + Send + 'static) -> Self {
        Self {
            f: Box::new(move |req| Ok::<_, Error>(f(req))),
            ..Default::default()
        }
    }

    pub fn run<E: HasPeerIdentity + std::fmt::Debug>(
        self,
    ) -> impl (FnMut(E) -> Result<BoxedIo, Error>) + Send + 'static {
        let Self {
            f,
            settings,
            identity,
        } = self;
        let f = Arc::new(Mutex::new(f));
        move |endpoint| {
            let span = tracing::debug_span!("server::run", ?endpoint);
            let _e = span.enter();
            if let Some(ref id) = identity {
                assert_eq!(&endpoint.peer_identity(), id)
            }
            let f = f.clone();
            let (client_io, server_io) = crate::io::duplex(4096);
            let svc = hyper::service::service_fn(move |request: Request<Body>| {
                let f = f.clone();
                async move {
                    tracing::info!(?request);
                    f.lock().unwrap()(request)
                }
            });
            tokio::spawn(settings.serve_connection(server_io, svc).in_current_span());
            Ok(BoxedIo::new(client_io))
        }
    }
}
