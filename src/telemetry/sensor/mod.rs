use std::sync::{Arc, Mutex};

use http::{Request, Response};
use tower_service::NewService;
use tower_h2::Body;

use ctx;
use telemetry::{event, metrics, tap};
use transparency::ClientError;

pub mod http;

pub use self::http::{Http, NewHttp};

#[derive(Clone, Debug)]
struct Inner {
    metrics: metrics::Record,
    taps: Arc<Mutex<tap::Taps>>,
}

/// Accepts events from sensors.
#[derive(Clone, Debug)]
struct Handle(Inner);

/// Supports the creation of telemetry scopes.
#[derive(Clone, Debug)]
pub struct Sensors(Inner);

impl Handle {
    fn send<F>(&mut self, mk: F)
    where
        F: FnOnce() -> event::Event,
    {
        let ev = mk();
        trace!("event: {:?}", ev);

        if let Ok(mut taps) = self.0.taps.lock() {
            taps.inspect(&ev);
        }

        self.0.metrics.record_event(&ev);
    }
}

impl Sensors {
    pub(super) fn new(metrics: metrics::Record, taps: &Arc<Mutex<tap::Taps>>) -> Self {
        Sensors(Inner {
            metrics,
            taps: taps.clone(),
        })
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(metrics::Record::for_test(), &Default::default())
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
