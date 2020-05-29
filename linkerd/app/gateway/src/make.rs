use super::gateway::Gateway;
use futures::{future, Future, Poll};
use linkerd2_app_core::proxy::api_resolve::Metadata;
use linkerd2_app_core::{dns, transport::tls, Error, NameAddr};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::IpAddr;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) struct MakeGateway<R, O> {
    suffixes: Arc<Vec<dns::Suffix>>,
    resolve: R,
    outbound: O,
}

impl<R, O> MakeGateway<R, O> {
    pub fn new(resolve: R, outbound: O, suffixes: impl IntoIterator<Item = dns::Suffix>) -> Self {
        Self {
            resolve,
            outbound,
            suffixes: Arc::new(suffixes.into_iter().collect()),
        }
    }
}

impl<R, O> tower::Service<inbound::Target> for MakeGateway<R, O>
where
    R: tower::Service<dns::Name, Response = (dns::Name, IpAddr)>,
    R::Error: Into<Error> + 'static,
    R::Future: Send + 'static,
    O: tower::Service<outbound::Logical<outbound::HttpEndpoint>> + Send + Clone + 'static,
    O::Response: Send + 'static,
    O::Future: Send + 'static,
    O::Error: Into<Error>,
{
    type Response = Gateway<O::Response>;
    type Error = Error;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: inbound::Target) -> Self::Future {
        let inbound::Target {
            dst_name,
            http_settings,
            tls_client_id,
            ..
        } = target;

        let source_identity = match tls_client_id {
            tls::Conditional::Some(id) => id,
            tls::Conditional::None(_) => {
                return Box::new(future::ok(Gateway::NoIdentity));
            }
        };

        let orig_dst = match dst_name {
            Some(n) => n,
            None => {
                return Box::new(future::ok(Gateway::NoAuthority));
            }
        };

        let suffixes = self.suffixes.clone();
        let mut outbound = self.outbound.clone();
        Box::new(
            // First, resolve the original name. This determines both the
            // canonical name as well as an IP that can be used as the outbound
            // original dst.
            self.resolve
                .call(orig_dst.name().clone())
                .map_err(Into::into)
                .and_then(move |(name, dst_ip)| {
                    tracing::debug!(%name, %dst_ip, "Resolved");
                    if !suffixes.iter().any(|s| s.contains(&name)) {
                        tracing::debug!(%name, ?suffixes, "No matches");
                        return future::Either::A(future::ok(Gateway::BadDomain(name)));
                    }

                    // Create an outbound target using the resolved IP & name.
                    let dst_addr = (dst_ip, orig_dst.port()).into();
                    let dst_name = NameAddr::new(name, orig_dst.port());
                    let endpoint = outbound::Logical {
                        addr: dst_name.clone().into(),
                        inner: outbound::HttpEndpoint {
                            addr: dst_addr,
                            settings: http_settings,
                            metadata: Metadata::empty(),
                            identity: tls::PeerIdentity::None(
                                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                            ),
                        },
                    };

                    future::Either::B(outbound.call(endpoint).map_err(Into::into).map(
                        move |outbound| Gateway::Outbound {
                            dst_addr,
                            dst_name,
                            outbound,
                            source_identity,
                        },
                    ))
                }),
        )
    }
}
