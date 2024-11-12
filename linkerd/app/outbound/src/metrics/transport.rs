use crate::{opaq, tls};
use linkerd_app_core::{
    io,
    metrics::prom::{self, encoding::*, registry::Registry, EncodeLabelSetMut, Family},
    svc::{layer, NewService, Param, Service, ServiceExt},
    Error,
};
use std::{fmt::Debug, hash::Hash};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub(crate) struct TransportRouteMetricsFamily<L> {
    open: Family<L, prom::Counter>,
    close: Family<ConnectionsClosedLabels<L>, prom::Counter>,
}

#[derive(Clone, Debug)]
struct TransportRouteMetrics {
    open: prom::Counter,
    close_no_err: prom::Counter,
    close_forbidden: prom::Counter,
    close_invalid_backend: prom::Counter,
    close_invalid_policy: prom::Counter,
    close_unexpected: prom::Counter,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum ErrorKind {
    Forbidden,
    InvalidBackend,
    InvalidPolicy,
    Unexpected,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ConnectionsClosedLabels<L> {
    labels: L,
    error: Option<ErrorKind>,
}

#[derive(Clone, Debug)]
pub(crate) struct NewTransportRouteMetrics<N, L: Clone> {
    inner: N,
    family: TransportRouteMetricsFamily<L>,
}

#[derive(Clone, Debug)]
pub(crate) struct TransportRouteMetricsService<T, N> {
    target: T,
    inner: N,
    metrics: TransportRouteMetrics,
}
// === impl TransportRouteMetricsFamily ===

impl<L> Default for TransportRouteMetricsFamily<L>
where
    L: EncodeLabelSetMut + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self {
            open: prom::Family::default(),
            close: prom::Family::default(),
        }
    }
}

impl<L> TransportRouteMetricsFamily<L>
where
    L: EncodeLabelSetMut + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub(crate) fn register(registry: &mut Registry) -> Self {
        let open = prom::Family::<L, prom::Counter>::default();
        registry.register("open", "The number of connections opened", open.clone());

        let close = prom::Family::<ConnectionsClosedLabels<L>, prom::Counter>::default();
        registry.register("close", "The number of connections closed", close.clone());

        Self { open, close }
    }

    fn closed_counter(&self, labels: &L, error: Option<ErrorKind>) -> prom::Counter {
        self.close
            .get_or_create(&ConnectionsClosedLabels {
                labels: labels.clone(),
                error,
            })
            .clone()
    }

    fn metrics(&self, labels: L) -> TransportRouteMetrics {
        TransportRouteMetrics {
            open: self.open.get_or_create(&labels).clone(),
            close_no_err: self.closed_counter(&labels, None),
            close_forbidden: self.closed_counter(&labels, Some(ErrorKind::Forbidden)),
            close_invalid_backend: self.closed_counter(&labels, Some(ErrorKind::InvalidBackend)),
            close_invalid_policy: self.closed_counter(&labels, Some(ErrorKind::InvalidPolicy)),
            close_unexpected: self.closed_counter(&labels, Some(ErrorKind::Unexpected)),
        }
    }
}

impl ErrorKind {
    fn mk(err: &(dyn std::error::Error + 'static)) -> Self {
        if err.is::<opaq::TCPForbiddenRoute>() {
            ErrorKind::Forbidden
        } else if err.is::<opaq::TCPInvalidBackend>() {
            ErrorKind::InvalidBackend
        } else if err.is::<opaq::TCPInvalidPolicy>() {
            ErrorKind::InvalidPolicy
        } else if err.is::<tls::TLSForbiddenRoute>() {
            ErrorKind::Forbidden
        } else if err.is::<tls::TLSInvalidBackend>() {
            ErrorKind::InvalidBackend
        } else if err.is::<tls::TLSInvalidPolicy>() {
            ErrorKind::InvalidPolicy
        } else if let Some(e) = err.source() {
            Self::mk(e)
        } else {
            ErrorKind::Unexpected
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden => write!(f, "forbidden"),
            Self::InvalidBackend => write!(f, "invalid_backend"),
            Self::InvalidPolicy => write!(f, "invalid_policy"),
            Self::Unexpected => write!(f, "unexpected"),
        }
    }
}

// === impl ConnectionsClosedLabels ===

impl<L> EncodeLabelSetMut for ConnectionsClosedLabels<L>
where
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
{
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        self.labels.encode_label_set(enc)?;
        match self.error {
            Some(error) => ("error", error.to_string()).encode(enc.encode_label())?,
            None => ("error", "").encode(enc.encode_label())?,
        }

        Ok(())
    }
}

impl<L> EncodeLabelSet for ConnectionsClosedLabels<L>
where
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
{
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl NewTransportRouteMetrics ===

impl<N, L: Clone> NewTransportRouteMetrics<N, L> {
    pub fn layer(
        family: TransportRouteMetricsFamily<L>,
    ) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            family: family.clone(),
        })
    }
}

impl<T, N, L> NewService<T> for NewTransportRouteMetrics<N, L>
where
    N: Clone,
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
    T: Param<L>,
{
    type Service = TransportRouteMetricsService<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let labels: L = target.param();
        let metrics = self.family.metrics(labels);
        TransportRouteMetricsService::new(target, self.inner.clone(), metrics)
    }
}

// === impl TransportRouteMetricsService ===

impl<T, N> TransportRouteMetricsService<T, N> {
    fn new(target: T, inner: N, metrics: TransportRouteMetrics) -> Self {
        Self {
            target,
            inner,
            metrics,
        }
    }
}

impl<T, I, N, S> Service<I> for TransportRouteMetricsService<T, N>
where
    T: Clone + Send + Sync + 'static,
    I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
    N: NewService<T, Service = S> + Clone + Send + 'static,
    S: Service<I> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let new_accept = self.inner.clone();
        let metrics = self.metrics.clone();

        Box::pin(async move {
            let svc = new_accept.new_service(target);
            metrics.inc_open();

            match svc.oneshot(io).await.map_err(Into::into) {
                Ok(result) => {
                    metrics.inc_closed(None);
                    Ok(result)
                }
                Err(error) => {
                    metrics.inc_closed(Some(&*error));
                    Err(error)
                }
            }
        })
    }
}

impl TransportRouteMetrics {
    fn inc_open(&self) {
        self.open.inc();
    }
    fn inc_closed(&self, err: Option<&(dyn std::error::Error + 'static)>) {
        match err.map(ErrorKind::mk) {
            Some(ErrorKind::Forbidden) => {
                self.close_forbidden.inc();
            }
            Some(ErrorKind::InvalidBackend) => {
                self.close_invalid_backend.inc();
            }
            Some(ErrorKind::InvalidPolicy) => {
                self.close_invalid_policy.inc();
            }
            Some(ErrorKind::Unexpected) => {
                self.close_unexpected.inc();
            }
            None => {
                self.close_no_err.inc();
            }
        }
    }
}
