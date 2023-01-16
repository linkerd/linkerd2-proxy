use super::opaque_transport;
use crate::{stack_labels, Outbound};
use linkerd_app_core::{
    drain, io, metrics,
    proxy::{api_resolve::Metadata, core::Resolve, http, tcp},
    svc::{self, NewService},
    tls,
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

impl svc::Param<tcp::balance::EwmaConfig> for Balance {
    fn param(&self) -> tcp::balance::EwmaConfig {
        tcp::balance::EwmaConfig {
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

impl svc::Param<Option<opaque_transport::PortOverride>> for Endpoint {
    fn param(&self) -> Option<opaque_transport::PortOverride> {
        self.metadata
            .opaque_transport_port()
            .map(opaque_transport::PortOverride)
    }
}

impl svc::Param<Option<transport_header::SessionProtocol>> for Endpoint {
    fn param(&self) -> Option<transport_header::SessionProtocol> {
        None
    }
}

impl svc::Param<tls::ConditionalClientTls> for Endpoint {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
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

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a [`svc::NewService`] stack that builds buffered opaque TCP load
    /// balancer services for [`Concrete`] targets.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    pub fn push_tcp_concrete<T, I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<policy::Backend>,
        T: Clone + Send + Sync + 'static,
        C: svc::MakeConnection<Endpoint> + Clone + Send + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
        C: Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        R: Clone + Send + Sync + 'static,
        R: Resolve<Balance, Endpoint = Metadata, Error = Error> + Sync,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, connect| {
            let crate::Config {
                tcp_connection_buffer,
                inbound_ips,
                ..
            } = config;

            let endpoint = connect
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_make_thunk();

            let balance = endpoint
                .clone()
                .check_new_service::<Endpoint, ()>()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("tcp", "endpoint")),
                )
                .instrument(|e: &Endpoint| info_span!("endpoint", addr = %e.addr))
                .push_new_clone()
                .check_new::<Endpoint>()
                .push(NewBalanceEndpoint::layer(config.inbound_ips.clone()))
                .check_new_new::<Balance, (SocketAddr, Metadata)>()
                .push(tcp::NewBalancePeakEwma::layer(resolve))
                .check_new::<Balance>();

            balance
                .check_new::<Balance>()
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
                    endpoint.check_new::<Endpoint>().into_inner(),
                )
                .check_new::<Params>()
                .push_on_service(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone()))
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("opaque", "concrete")),
                        )
                        // TODO(ver) configure buffer from target
                        .push_buffer("Opaque Concrete", tcp_connection_buffer),
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
    N: NewService<T>,
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
    N: NewService<Endpoint>,
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
