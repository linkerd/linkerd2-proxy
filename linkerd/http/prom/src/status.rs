//! A tower middleware for counting response status codes.

use crate::{
    record_response::RequestCancelled,
    stream_label::{LabelSet, MkStreamLabel, StreamLabel},
};
use http::{Request, Response};
use http_body::Body;
use linkerd_error::Error;
use linkerd_http_body_eos::{BodyWithEosFn, EosRef};
use linkerd_metrics::prom::{Counter, Family, Registry};
use linkerd_stack::{self as svc, layer::Layer, ExtractParam, NewService, Service};
use pin_project::pin_project;
use std::{
    future::Future,
    hash::Hash,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(test)]
mod tests;

/// Wraps a [`NewService<T>`] with response status code metrics.
pub struct NewRecordStatusCode<N, X, ML, L> {
    /// The inner wrapped service generator.
    mk_svc: N,
    /// Extracts an `ML`-typed [`MkStreamLabel`] from targets.
    extract: X,
    /// Marker indicating the `ML`-typed [`MkStreamLabel`] extracted from targets.
    labeler: PhantomData<ML>,
    /// Marker indicating the labels for the [`Family<L>`] extracted from targets.
    label: PhantomData<L>,
}

/// Wraps a [`Service<T>`] with response status code metrics.
#[derive(Clone)]
pub struct RecordStatusCode<S, ML, L> {
    /// The inner wrapped service.
    svc: S,
    /// A [`MkStreamLabel`] implementation to label the service's traffic.
    ///
    /// This generates an `L`-typed label set to acquire a [`Counter`] from the metrics family.
    mk_stream_label: ML,
    /// The [`Family`] of Prometheus [`Counter`]s tracking status codes.
    metrics: StatusMetrics<L>,
}

/// A [`Future`] returned by a [`RecordStatusCode<S>`] service.
#[pin_project(project = RecordStatusFutureProj)]
pub enum RecordStatusFuture<F, SL, L> {
    /// A transparent [`RecordStatusFuture`].
    ///
    /// This means that the [`MkStreamLabel`] did not emit a [`StreamLabel`], and that the
    /// request/response pair did not need to be recorded.
    Passthru(#[pin] F),
    /// An instrumented [`RecordStatusFuture`].
    ///
    /// This will use the `SL`-typed [`StreamLabel`] type to label the traffic.
    Instrumented {
        /// The inner wrapped future.
        #[pin]
        fut: F,
        stream_label: Option<SL>,
        /// A [`Family`] of labeled counters.
        metrics: StatusMetrics<L>,
    },
}

/// Parameters for [`NewRecordStatusCode<S, ML, L>`] services.
pub struct Params<ML, L> {
    /// A [`MkStreamLabel`] implementation to label the service's traffic.
    ///
    /// This generates an `L`-typed label set to acquire a [`Counter`] from the metrics family.
    pub mk_stream_label: ML,
    /// The [`Family`] of Prometheus [`Counter`]s tracking status codes.
    pub metrics: StatusMetrics<L>,
}

/// Prometheus metrics for [`NewRecordStatusCode<N, X, ML, L>`].
#[derive(Clone, Debug)]
pub struct StatusMetrics<L> {
    counters: CounterFamily<L>,
}

/// A [`Family`] of labeled counters.
type CounterFamily<L> = Family<L, Counter>;

/// A [`Body`] returned by [`RecordStatusFuture`].
pub type RecordStatusBody<B> = http_body_util::Either<InstrumentedBody<B>, B>;

/// A [`Body`] that will invoke a closure upon reaching the end of the stream.
type InstrumentedBody<B> = BodyWithEosFn<B, EosCallback>;

/// A boxed callback used by [`InstrumentedBody<B>`] to inspect the end of the stream.
type EosCallback = Box<dyn FnOnce(EosRef<'_, Error>) + Send>;

// === impl NewRecordStatusCode ===

impl<N, X, ML, L> NewRecordStatusCode<N, X, ML, L>
where
    X: Clone,
{
    /// Returns a [`Layer`] that can be applied to an inner [`NewService<T>`].
    pub fn layer_via(extract: X) -> impl Layer<N, Service = Self> {
        svc::layer::mk(move |inner| Self {
            mk_svc: inner,
            extract: extract.clone(),
            labeler: PhantomData,
            label: PhantomData,
        })
    }

    /// A helper to confirm that this type can be used as a [`NewService<T>`].
    ///
    /// This helps provide friendlier error messages.
    pub fn check_new_service<T>(self) -> Self
    where
        N: NewService<T>,
        X: ExtractParam<Params<ML, L>, T>,
    {
        self
    }
}

impl<N, T, L, ML, X> NewService<T> for NewRecordStatusCode<N, X, ML, L>
where
    N: NewService<T>,
    X: ExtractParam<Params<ML, L>, T>,
{
    type Service = RecordStatusCode<N::Service, ML, L>;

    fn new_service(&self, target: T) -> Self::Service {
        let Self {
            mk_svc,
            extract,
            labeler: _,
            label: _,
        } = self;

        // Extract a stream labeler and a family of counters.
        let Params {
            mk_stream_label,
            metrics,
        } = extract.extract_param(&target);
        let svc = mk_svc.new_service(target);

        RecordStatusCode {
            svc,
            mk_stream_label,
            metrics,
        }
    }
}

// === impl RecordStatusCode ===

impl<S, ML, L, ReqB, RspB> Service<Request<ReqB>> for RecordStatusCode<S, ML, L>
where
    S: Service<Request<ReqB>, Response = Response<RspB>, Error = Error>,
    S::Error: Into<Error>,
    ML: MkStreamLabel<StatusLabels = L>,
    RspB: Body<Error = Error>,
    L: Clone + Hash + Eq + Send + Sync + 'static,
{
    type Response = Response<RecordStatusBody<RspB>>;
    type Error = Error;
    type Future = RecordStatusFuture<S::Future, ML::StreamLabel, ML::StatusLabels>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let Self {
            svc,
            mk_stream_label: _,
            metrics: _,
        } = self;

        svc.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let Self {
            svc,
            mk_stream_label,
            metrics,
        } = self;

        if let stream_label @ Some(_) = mk_stream_label.mk_stream_labeler(&req) {
            // If this request should be recorded, return an instrumented future.
            let fut = svc.call(req);
            let metrics = metrics.clone();
            RecordStatusFuture::Instrumented {
                fut,
                stream_label,
                metrics,
            }
        } else {
            let fut = svc.call(req);
            RecordStatusFuture::Passthru(fut)
        }
    }
}

// === impl RecordStatusFuture ===

impl<F, B, SL, L> Future for RecordStatusFuture<F, SL, L>
where
    F: Future<Output = Result<Response<B>, Error>>,
    B: Body<Error = Error>,
    SL: StreamLabel<StatusLabels = L>,
    L: Clone + Hash + Eq + Send + Sync + 'static,
{
    type Output = Result<Response<RecordStatusBody<B>>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        use http_body_util::Either::{Left as Enabled, Right as Disabled};

        match self.project() {
            // If this is a transparent future, poll the future and return the response.
            RecordStatusFutureProj::Passthru(fut) => fut.poll(cx).map_ok(|rsp| rsp.map(Disabled)),
            // If this reponse should be recorded, instrument the response when it is ready.
            RecordStatusFutureProj::Instrumented {
                fut,
                stream_label,
                metrics,
            } => {
                // Wait for the inner future. Once ready, take the labeler and metric family.
                let rsp = futures::ready!(fut.poll(cx));
                let metrics = metrics.clone();
                let mut stream_label = stream_label.take().expect("futures only yield once");

                let rsp = match rsp {
                    Ok(rsp) => {
                        // Observe the start of the response, and then instrument the response
                        // body with a callback that will record the outcome once finished.
                        stream_label.init_response(&rsp);
                        let on_eos =
                            move |eos: EosRef<'_>| Self::on_eos(eos, stream_label, metrics);
                        let instrument = |body| InstrumentedBody::new(body, Box::new(on_eos));
                        Ok(rsp.map(instrument).map(Enabled))
                    }
                    Err(err) => {
                        // Record an error if there is no response.
                        Self::on_eos(EosRef::Error(&err), stream_label, metrics);
                        Err(err)
                    }
                };

                Poll::Ready(rsp)
            }
        }
    }
}

impl<F, SL, L> RecordStatusFuture<F, SL, L>
where
    SL: StreamLabel<StatusLabels = L>,
    L: Clone + Hash + Eq + Send + Sync + 'static,
{
    fn on_eos(eos: EosRef<'_, Error>, mut stream_label: SL, metrics: StatusMetrics<L>) {
        let ugh = RequestCancelled.into(); // XXX(kate)

        stream_label.end_response(match eos {
            EosRef::None => Ok(None),
            EosRef::Trailers(trls) => Ok(Some(trls)),
            EosRef::Error(error) => Err(error),
            EosRef::Cancelled => Err(&ugh),
        });

        let labels = stream_label.status_labels();
        let counter = metrics.metric(&labels);
        counter.inc();
    }
}

// === impl StatusMetrics ===

impl<L> Default for StatusMetrics<L>
where
    L: LabelSet,
{
    fn default() -> Self {
        Self {
            counters: Default::default(),
        }
    }
}

impl<L> StatusMetrics<L>
where
    L: LabelSet,
{
    /// Registers a new [`StatusMetrics<L>`] with the given metrics registry.
    pub fn register(registry: &mut Registry, help: &'static str) -> Self {
        let counters = Family::default();

        registry.register("statuses", help, counters.clone());

        Self { counters }
    }
}

impl<L> StatusMetrics<L>
where
    L: Clone + Hash + Eq,
{
    pub fn metric(&self, labels: &L) -> Counter {
        let Self { counters } = self;

        counters.get_or_create(labels).to_owned()
    }
}
