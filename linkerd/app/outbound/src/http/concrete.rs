//! A stack that (optionally) resolves a service to a set of endpoint replicas
//! and distributes HTTP requests among them.

use super::{balance::EwmaConfig, client, handle_proxy_error_headers};
use crate::{http, stack_labels, BackendRef, Outbound, ParentRef};
use linkerd_app_core::{
    config::{ConnectConfig, QueueConfig},
    metrics::{prefix_labels, EndpointLabels, OutboundEndpointLabels},
    profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata, ProtocolHint},
        core::Resolve,
        tap,
    },
    svc, tls,
    transport::{self, addrs::*},
    Error, Infallible, NameAddr, Result,
};
use linkerd_proxy_client_policy::FailureAccrual;
use std::{fmt::Debug, net::SocketAddr, sync::Arc};
use tracing::info_span;

mod balance;

pub use self::balance::BalancerMetrics;

/// Parameter configuring dispatcher behavior.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dispatch {
    Balance(NameAddr, EwmaConfig),
    Forward(Remote<ServerAddr>, Metadata),
    Fail { message: Arc<str> },
}

/// A backend dispatcher explicitly fails all requests.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DispatcherFailed(Arc<str>);

/// Inner stack target type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint<T> {
    addr: Remote<ServerAddr>,
    is_local: bool,
    metadata: Metadata,
    parent: T,
    queue: QueueConfig,
    close_server_connection_on_remote_proxy_error: bool,

    http1: http::h1::PoolSettings,
    http2: http::h2::ClientParams,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a [`svc::NewService`] stack that builds buffered HTTP services
    /// for `T`-typed concrete targets. Requests may be load balanced across a
    /// discovered set of replicas or forwarded to a single endpoint, depending
    /// on the value of the `Dispatch` parameter.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    pub fn push_http_concrete<T, NSvc, R>(self, resolve: R) -> Outbound<svc::ArcNewCloneHttp<T>>
    where
        // Concrete target type.
        T: svc::Param<ParentRef>,
        T: svc::Param<BackendRef>,
        T: svc::Param<Dispatch>,
        T: svc::Param<FailureAccrual>,
        T: Clone + Debug + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata>,
        R::Resolution: Unpin,
        // Endpoint stack.
        N: svc::NewService<Endpoint<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, inner| {
            let inbound_ips = config.inbound_ips.clone();

            // TODO(ver) Configure this from discovery.
            let queue = config.http_request_queue;

            let forward = inner
                .clone()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "forward")),
                )
                .push(svc::NewQueue::layer())
                .instrument(|e: &Endpoint<T>| info_span!("forward", addr = %e.addr));

            let fail = svc::ArcNewService::new(|message: Arc<str>| {
                svc::mk(move |_| futures::future::ready(Err(DispatcherFailed(message.clone()))))
            });

            let ConnectConfig { http1, http2, .. } = config.proxy.connect.clone();

            inner
                .push(balance::Balance::layer(config, rt, resolve))
                .check_new_clone()
                .push_switch(Ok::<_, Infallible>, forward.into_inner())
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Dispatch::Balance(addr, ewma) => {
                                svc::Either::A(svc::Either::A(balance::Balance {
                                    addr,
                                    ewma,
                                    parent,
                                    queue,
                                }))
                            }
                            Dispatch::Forward(addr, metadata) => svc::Either::A(svc::Either::B({
                                let is_local = inbound_ips.contains(&addr.ip());
                                let http2 = http2.override_from(metadata.http2_client_params());
                                Endpoint {
                                    is_local,
                                    addr,
                                    metadata,
                                    parent,
                                    queue,
                                    close_server_connection_on_remote_proxy_error: true,
                                    http1,
                                    http2,
                                }
                            })),
                            Dispatch::Fail { message } => svc::Either::B(message),
                        })
                    },
                    svc::stack(fail).check_new_clone().into_inner(),
                )
                .arc_new_clone_http()
        })
    }
}

// === impl Endpoint ===

impl<T> svc::Param<Remote<ServerAddr>> for Endpoint<T> {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl<T> svc::Param<svc::queue::Capacity> for Endpoint<T> {
    fn param(&self) -> svc::queue::Capacity {
        svc::queue::Capacity(self.queue.capacity)
    }
}

impl<T> svc::Param<svc::queue::Timeout> for Endpoint<T> {
    fn param(&self) -> svc::queue::Timeout {
        svc::queue::Timeout(self.queue.failfast_timeout)
    }
}

impl<T> svc::Param<Option<crate::tcp::tagged_transport::PortOverride>> for Endpoint<T> {
    fn param(&self) -> Option<crate::tcp::tagged_transport::PortOverride> {
        if self.is_local {
            return None;
        }
        self.metadata
            .tagged_transport_port()
            .map(crate::tcp::tagged_transport::PortOverride)
    }
}

impl<T> svc::Param<Option<http::AuthorityOverride>> for Endpoint<T> {
    fn param(&self) -> Option<http::AuthorityOverride> {
        if self.is_local {
            return None;
        }
        self.metadata
            .authority_override()
            .cloned()
            .map(http::AuthorityOverride)
    }
}

impl<T> svc::Param<handle_proxy_error_headers::CloseServerConnection> for Endpoint<T> {
    fn param(&self) -> handle_proxy_error_headers::CloseServerConnection {
        handle_proxy_error_headers::CloseServerConnection(
            self.close_server_connection_on_remote_proxy_error,
        )
    }
}

impl<T> svc::Param<transport::labels::Key> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl<T> svc::Param<OutboundEndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> OutboundEndpointLabels {
        OutboundEndpointLabels {
            authority: self.parent.param(),
            labels: prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.param(),
            target_addr: self.addr.into(),
        }
    }
}

impl<T> svc::Param<EndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> EndpointLabels {
        EndpointLabels::Outbound(self.param())
    }
}

impl<T> svc::Param<tls::ConditionalClientTls> for Endpoint<T> {
    fn param(&self) -> tls::ConditionalClientTls {
        if self.is_local {
            return tls::ConditionalClientTls::None(tls::NoClientTls::Loopback);
        }

        // If we're transporting an opaque protocol OR we're communicating with
        // a gateway, then set an ALPN value indicating support for a transport
        // header.
        let use_transport_header = self.metadata.tagged_transport_port().is_some()
            || self.metadata.authority_override().is_some();
        self.metadata
            .identity()
            .cloned()
            .map(move |mut client_tls| {
                client_tls.alpn = if use_transport_header {
                    use linkerd_app_core::transport_header::PROTOCOL;
                    Some(tls::client::AlpnProtocols(vec![PROTOCOL.into()]))
                } else {
                    None
                };

                tls::ConditionalClientTls::Some(client_tls)
            })
            .unwrap_or(tls::ConditionalClientTls::None(
                tls::NoClientTls::NotProvidedByServiceDiscovery,
            ))
    }
}

impl<T> svc::Param<http::Version> for Endpoint<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> http::Version {
        self.parent.param()
    }
}

impl<T> svc::Param<client::Params> for Endpoint<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> client::Params {
        match self.param() {
            http::Version::H2 => client::Params::H2(self.http2.clone()),
            http::Version::Http1 => {
                // When the target is local (i.e. same as source of traffic)
                // then do not perform a protocol upgrade to HTTP/2
                if self.is_local {
                    return client::Params::Http1(self.http1);
                }
                match self.metadata.protocol_hint() {
                    // If the protocol hint is unknown or indicates that the
                    // endpoint's proxy will treat connections as opaque, do not
                    // perform a protocol upgrade to HTTP/2.
                    ProtocolHint::Unknown | ProtocolHint::Opaque => {
                        client::Params::Http1(self.http1)
                    }
                    ProtocolHint::Http2 => {
                        client::Params::OrigProtoUpgrade(self.http2.clone(), self.http1)
                    }
                }
            }
        }
    }
}

impl<T> svc::Param<ProtocolHint> for Endpoint<T> {
    fn param(&self) -> ProtocolHint {
        self.metadata.protocol_hint()
    }
}

// TODO(ver) move this into the endpoint stack?
impl<T> tap::Inspect for Endpoint<T> {
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
        svc::Param::<tls::ConditionalClientTls>::param(self)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<tap::Labels> {
        // FIXME(ver) create a dedicated extension type for route labels.
        req.extensions()
            .get::<profiles::http::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}
