use std::sync::{Arc, Mutex};

use http::{Request, Response};
use tower_service::Service;
use tower_h2::Body;

use ctx;
use telemetry::{http::event, tap};
use proxy::ClientError;

use super::record::Record;
use super::service::{Http, RequestBody};

#[derive(Clone, Debug)]
struct Inner {
    metrics: Record,
    taps: Arc<Mutex<tap::Taps>>,
}

/// Accepts events from sensors.
#[derive(Clone, Debug)]
pub(super) struct Handle(Inner);

/// Supports the creation of telemetry scopes.
#[derive(Clone, Debug)]
pub struct Sensors(Inner);

impl Handle {
    pub fn send<F>(&mut self, mk: F)
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
    pub(super) fn new(metrics: Record, taps: &Arc<Mutex<tap::Taps>>) -> Self {
        Sensors(Inner {
            metrics,
            taps: taps.clone(),
        })
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(Record::for_test(), &Default::default())
    }

    pub fn http<S, A, B>(
        &self,
        client_ctx: Arc<ctx::transport::Client>,
        service: S,
    ) -> Http<S, A, B>
    where
        A: Body + 'static,
        B: Body + 'static,
        S: Service<
            Request = Request<RequestBody<A>>,
            Response = Response<B>,
            Error = ClientError
        >
            + 'static,
    {
        Http::new(service, Handle(self.0.clone()), client_ctx)
    }
}
