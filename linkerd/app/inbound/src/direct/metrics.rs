use super::ClientInfo;
use linkerd_app_core::{
    metrics::prom::{self, EncodeLabelSetMut},
    svc, tls,
    transport_header::{SessionProtocol, TransportHeader},
};

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct NewRecord<N> {
    inner: N,
    metrics: MetricsFamilies,
}

#[derive(Clone, Debug, Default)]
pub struct MetricsFamilies {
    connections: prom::Family<Labels, prom::Counter>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Labels {
    header: TransportHeader,
    client_id: tls::ClientId,
}

impl MetricsFamilies {
    pub fn register(reg: &mut prom::Registry) -> Self {
        let connections = prom::Family::default();
        reg.register(
            "connections",
            "TCP connections with transport headers",
            connections.clone(),
        );

        Self { connections }
    }
}

impl<N> NewRecord<N> {
    pub fn layer(metrics: MetricsFamilies) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            metrics: metrics.clone(),
        })
    }
}

impl<N> svc::NewService<(TransportHeader, ClientInfo)> for NewRecord<N>
where
    N: svc::NewService<(TransportHeader, ClientInfo)>,
{
    type Service = N::Service;

    fn new_service(&self, (header, client): (TransportHeader, ClientInfo)) -> Self::Service {
        self.metrics
            .connections
            .get_or_create(&Labels {
                header: header.clone(),
                client_id: client.client_id.clone(),
            })
            .inc();

        self.inner.new_service((header, client))
    }
}

impl prom::EncodeLabelSetMut for Labels {
    fn encode_label_set(&self, enc: &mut prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        use prom::encoding::EncodeLabel;
        (
            "session_protocol",
            self.header.protocol.as_ref().map(|p| match p {
                SessionProtocol::Http1 => "http/1",
                SessionProtocol::Http2 => "http/2",
            }),
        )
            .encode(enc.encode_label())?;
        ("target_port", self.header.port).encode(enc.encode_label())?;
        ("target_name", self.header.name.as_deref()).encode(enc.encode_label())?;
        ("client_id", self.client_id.to_str()).encode(enc.encode_label())?;
        Ok(())
    }
}

impl prom::encoding::EncodeLabelSet for Labels {
    fn encode(&self, mut enc: prom::encoding::LabelSetEncoder<'_>) -> Result<(), std::fmt::Error> {
        self.encode_label_set(&mut enc)
    }
}
