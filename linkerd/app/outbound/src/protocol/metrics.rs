use super::Protocol;
use crate::ParentRef;
use linkerd_app_core::{
    metrics::prom::{self, EncodeLabelSetMut},
    svc,
};

#[derive(Clone, Debug)]
pub struct NewRecord<N> {
    inner: N,
    metrics: MetricsFamilies,
}

#[derive(Clone, Debug)]
pub struct Record<S> {
    inner: S,
    counter: prom::Counter,
}

#[derive(Clone, Debug, Default)]
pub struct MetricsFamilies {
    connections: prom::Family<Labels, prom::Counter>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Labels {
    protocol: Protocol,
    parent_ref: ParentRef,
}

// === impl MetricsFamilies ===

impl MetricsFamilies {
    pub fn register(reg: &mut prom::Registry) -> Self {
        let connections = prom::Family::default();
        reg.register(
            "connections",
            "Outbound TCP connections by protocol configuration",
            connections.clone(),
        );

        Self { connections }
    }
}

// === impl NewRecord ===

impl<N> NewRecord<N> {
    pub fn layer(metrics: MetricsFamilies) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            metrics: metrics.clone(),
        })
    }
}

impl<T, N> svc::NewService<T> for NewRecord<N>
where
    T: svc::Param<Protocol>,
    T: svc::Param<ParentRef>,
    N: svc::NewService<T>,
{
    type Service = Record<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let counter = (*self.metrics.connections.get_or_create(&Labels {
            protocol: target.param(),
            parent_ref: target.param(),
        }))
        .clone();

        let inner = self.inner.new_service(target);
        Record { inner, counter }
    }
}

// === impl Record ===

impl<S, I> svc::Service<I> for Record<S>
where
    S: svc::Service<I>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, io: I) -> Self::Future {
        self.counter.inc();
        self.inner.call(io)
    }
}

// === impl Labels ===

impl prom::EncodeLabelSetMut for Labels {
    fn encode_label_set(&self, enc: &mut prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        use prom::encoding::EncodeLabel;

        let protocol = match self.protocol {
            Protocol::Http1 => "http/1",
            Protocol::Http2 => "http/2",
            Protocol::Detect => "detect",
            Protocol::Opaque => "opaq",
            Protocol::Tls => "tls",
        };

        ("protocol", protocol).encode(enc.encode_label())?;
        self.parent_ref.encode_label_set(enc)?;

        Ok(())
    }
}

impl prom::encoding::EncodeLabelSet for Labels {
    fn encode(&self, mut enc: prom::encoding::LabelSetEncoder<'_>) -> Result<(), std::fmt::Error> {
        self.encode_label_set(&mut enc)
    }
}
