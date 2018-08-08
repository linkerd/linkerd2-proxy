use std::sync::{Arc, Mutex};
use std::time::Instant;

use http::{Request, Response};
use tokio_connect;
use tokio::io::{AsyncRead, AsyncWrite};
use tower_service::NewService;
use tower_h2::Body;

use ctx;
use telemetry::{event, metrics, tap};
use transport::Connection;
use transparency::ClientError;

pub mod http;
mod transport;

pub use self::http::{Http, NewHttp};
pub use self::transport::{Connect, Transport};

#[derive(Clone, Debug)]
struct Inner {
    metrics: metrics::Record,
    taps: Arc<Mutex<tap::Taps>>,
}

/// Accepts events from sensors.
#[derive(Clone, Debug)]
struct Handle(Option<Inner>);

/// Supports the creation of telemetry scopes.
#[derive(Clone, Debug)]
pub struct Sensors(Option<Inner>);

impl Handle {
    fn send<F>(&mut self, mk: F)
    where
        F: FnOnce() -> event::Event,
    {
        if let Some(inner) = self.0.as_mut() {
            let ev = mk();
            trace!("event: {:?}", ev);

            if let Ok(mut taps) = inner.taps.lock() {
                taps.inspect(&ev);
            }

            inner.metrics.record_event(&ev);
        }
    }
}

impl Sensors {
    pub(super) fn new(metrics: metrics::Record, taps: &Arc<Mutex<tap::Taps>>) -> Self {
        Sensors(Some(Inner {
            metrics,
            taps: taps.clone(),
        }))
    }

    pub fn null() -> Sensors {
        Sensors(None)
    }

    pub fn accept<T>(
        &self,
        io: T,
        opened_at: Instant,
        ctx: &Arc<ctx::transport::Server>,
    ) -> Transport<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        debug!("server connection open");
        let ctx = Arc::new(ctx::transport::Ctx::Server(Arc::clone(ctx)));
        Transport::open(io, opened_at, Handle(self.0.clone()), ctx)
    }

    pub fn connect<C>(&self, connect: C, ctx: &Arc<ctx::transport::Client>) -> Connect<C>
    where
        C: tokio_connect::Connect<Connected = Connection>,
    {
        Connect::new(connect, Handle(self.0.clone()), ctx)
    }

    pub fn http<N, A, B>(
        &self,
        new_service: N,
        client_ctx: &Arc<ctx::transport::Client>,
    ) -> NewHttp<N, A, B>
    where
        A: Body + 'static,
        B: Body + 'static,
        N: NewService<
            Request = Request<http::RequestBody<A>>,
            Response = Response<B>,
            Error = ClientError
        >
            + 'static,
    {
        NewHttp::new(new_service, Handle(self.0.clone()), client_ctx)
    }
}
