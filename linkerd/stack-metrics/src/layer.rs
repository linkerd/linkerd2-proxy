use crate::{Metrics, TrackService};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct TrackServiceLayer(Arc<Metrics>);

impl TrackServiceLayer {
    pub(crate) fn new(metrics: Arc<Metrics>) -> Self {
        TrackServiceLayer(metrics)
    }
}

impl<S> tower::layer::Layer<S> for TrackServiceLayer {
    type Service = TrackService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        self.0.create_total.incr();
        TrackService::new(inner, self.0.clone())
    }
}
