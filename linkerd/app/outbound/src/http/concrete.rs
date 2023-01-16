use crate::{stack_labels, tcp::opaque_transport, Outbound};
use linkerd_app_core::{
    metrics,
    proxy::{api_resolve::Metadata, core::Resolve, http, tap},
    svc, tls,
    transport::{self, Remote, ServerAddr},
    transport_header, Error, Infallible,
};
use linkerd_proxy_client_policy::{self as policy};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tracing::info_span;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params {
    backend: policy::Backend,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Balance {
    ewma: policy::PeakEwma,
    destination_get_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    addr: Remote<ServerAddr>,
    tls: tls::ConditionalClientTls,
    metadata: Metadata,
}

#[derive(Clone, Debug)]
struct NewBalanceEndpoint<N> {
    inbound_ips: Arc<HashSet<IpAddr>>,
    inner: N,
}

#[derive(Clone, Debug)]
struct NewEndpoint<N> {
    inbound_ips: Arc<HashSet<IpAddr>>,
    inner: N,
}

// === impl Params ===

impl Params {
    fn new<T>(target: T) -> Self
    where
        T: svc::Param<policy::Backend>,
    {
        Self {
            backend: target.param(),
        }
    }
}

// === impl Balance ===

impl svc::Param<http::balance::EwmaConfig> for Balance {
    fn param(&self) -> http::balance::EwmaConfig {
        http::balance::EwmaConfig {
            decay: self.ewma.decay,
            default_rtt: self.ewma.default_rtt,
        }
    }
}

// === impl Endpoint ===

impl svc::Param<Remote<ServerAddr>> for Endpoint {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl svc::Param<tls::ConditionalClientTls> for Endpoint {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
    }
}

impl svc::Param<Option<opaque_transport::PortOverride>> for Endpoint {
    fn param(&self) -> Option<opaque_transport::PortOverride> {
        self.metadata
            .opaque_transport_port()
            .map(opaque_transport::PortOverride)
    }
}

impl svc::Param<Option<http::AuthorityOverride>> for Endpoint {
    fn param(&self) -> Option<http::AuthorityOverride> {
        self.metadata
            .authority_override()
            .cloned()
            .map(http::AuthorityOverride)
    }
}

impl svc::Param<transport::labels::Key> for Endpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl svc::Param<metrics::OutboundEndpointLabels> for Endpoint {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let authority = None; // FIXME
        metrics::OutboundEndpointLabels {
            authority,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.tls.clone(),
            target_addr: self.addr.into(),
        }
    }
}

impl svc::Param<metrics::EndpointLabels> for Endpoint {
    fn param(&self) -> metrics::EndpointLabels {
        svc::Param::<metrics::OutboundEndpointLabels>::param(self).into()
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<http::ClientHandle>().map(|c| c.addr)
    }

    fn src_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::None(tls::NoServerTls::Loopback)
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr.into())
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        self.tls.clone()
    }

    // FIXME
    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<tap::Labels> {
        // req.extensions()
        //     .get::<profiles::http::Route>()
        //     .map(|r| r.labels().clone())
        None
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a [`svc::NewService`] stack that builds buffered HTTP load
    /// balancer services for [`Concrete`] targets.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    //
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_http_concrete<T, NSvc, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        T: svc::Param<policy::Backend>,
        T: Clone + Send + Sync + 'static,
        N: svc::NewService<Endpoint, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        R: Clone + Send + Sync + 'static,
        R: Resolve<Balance, Error = Error, Endpoint = Metadata>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, endpoint| {
            let crate::Config {
                tcp_connection_buffer,
                inbound_ips,
                ..
            } = config;

            let balance = endpoint
                .clone()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "endpoint")),
                )
                .instrument(|e: &Endpoint| info_span!("endpoint", addr = %e.addr))
                .push_new_clone()
                .push(NewBalanceEndpoint::layer(inbound_ips.clone()))
                .push(http::NewBalancePeakEwma::layer(resolve))
                .check_new_service::<Balance, http::Request<_>>()
                .push_on_service(http::BoxResponse::layer());

            balance
                .check_new_service::<Balance, http::Request<_>>()
                .push_switch(
                    {
                        let inbound_ips = inbound_ips.clone();
                        move |Params { backend }| -> Result<_, Infallible> {
                            Ok(match backend.dispatcher {
                                policy::BackendDispatcher::BalanceP2c(
                                    policy::Load::PeakEwma(ewma),
                                    policy::EndpointDiscovery::DestinationGet { path },
                                ) => svc::Either::A(Balance {
                                    ewma,
                                    destination_get_path: path,
                                }),

                                policy::BackendDispatcher::Forward(addr, mut metadata) => {
                                    let tls = if inbound_ips.contains(&addr.ip()) {
                                        metadata.clear_upgrade();
                                        tracing::debug!(%addr, "Target is local");
                                        tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
                                    } else {
                                        client_tls(&metadata)
                                    };
                                    svc::Either::B(Endpoint {
                                        addr: Remote(ServerAddr(addr)),
                                        metadata,
                                        tls,
                                    })
                                }
                            })
                        }
                    },
                    endpoint
                        .check_new_service::<Endpoint, http::Request<_>>()
                        .into_inner(),
                )
                .check_new_service::<Params, http::Request<_>>()
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "concrete")),
                        )
                        // TODO(ver) configure buffer from target
                        .push_buffer("HTTP Concrete", &config.http_request_buffer),
                )
                .check_new::<Params>()
                .instrument(
                    |p: &Params| info_span!("concrete", backend.name = %p.backend.meta.name()),
                )
                .check_new::<Params>()
                .push_map_target(Params::new)
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl NewBalanceEndpoint ===

impl<N> NewBalanceEndpoint<N> {
    fn layer(inbound_ips: Arc<HashSet<IpAddr>>) -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inbound_ips: inbound_ips.clone(),
            inner,
        })
    }
}

impl<T, N> svc::NewService<T> for NewBalanceEndpoint<N>
where
    N: svc::NewService<T>,
{
    type Service = NewEndpoint<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        NewEndpoint {
            inner: self.inner.new_service(target),
            inbound_ips: self.inbound_ips.clone(),
        }
    }
}

// === impl NewEndpoint ===

impl<N> svc::NewService<(SocketAddr, Metadata)> for NewEndpoint<N>
where
    N: svc::NewService<Endpoint>,
{
    type Service = N::Service;

    fn new_service(&self, (addr, mut metadata): (SocketAddr, Metadata)) -> Self::Service {
        let tls = if self.inbound_ips.contains(&addr.ip()) {
            metadata.clear_upgrade();
            tracing::debug!(%addr, ?metadata, ?addr, ?self.inbound_ips, "Target is local");
            tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
        } else {
            client_tls(&metadata)
        };

        let endpoint = Endpoint {
            addr: Remote(ServerAddr(addr)),
            metadata,
            tls,
        };
        self.inner.new_service(endpoint)
    }
}

fn client_tls(metadata: &Metadata) -> tls::ConditionalClientTls {
    // If we're transporting an opaque protocol OR we're communicating with
    // a gateway, then set an ALPN value indicating support for a transport
    // header.
    let use_transport_header =
        metadata.opaque_transport_port().is_some() || metadata.authority_override().is_some();

    metadata
        .identity()
        .cloned()
        .map(move |server_id| {
            tls::ConditionalClientTls::Some(tls::ClientTls {
                server_id,
                alpn: if use_transport_header {
                    Some(tls::client::AlpnProtocols(vec![
                        transport_header::PROTOCOL.into()
                    ]))
                } else {
                    None
                },
            })
        })
        .unwrap_or(tls::ConditionalClientTls::None(
            tls::NoClientTls::NotProvidedByServiceDiscovery,
        ))
}
