use futures::prelude::*;
use linkerd2_proxy_api::inbound::{
    self as api, inbound_server_policies_client::InboundServerPoliciesClient as ApiClient,
};
use linkerd_app_core::{
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::http,
    svc::Service,
    Error, IpNet, Recover, Result,
};
use linkerd_server_policy::{
    Authentication, Authorization, Network, Protocol, ServerPolicy, Suffix,
};
use linkerd_tonic_watch::StreamWatch;
use std::{convert::TryInto, sync::Arc};

#[derive(Clone, Debug)]
pub(super) struct Discover<S> {
    workload: String,
    client: ApiClient<S>,
}

#[derive(Clone)]
pub(super) struct GrpcRecover(ExponentialBackoff);

pub(super) type Watch<S> = StreamWatch<GrpcRecover, Discover<S>>;

impl<S> Discover<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
{
    pub(super) fn new(workload: String, client: S) -> Self {
        Self {
            workload,
            client: ApiClient::new(client),
        }
    }

    pub(super) fn into_watch(self, backoff: ExponentialBackoff) -> Watch<S> {
        StreamWatch::new(GrpcRecover(backoff), self)
    }
}

impl<S> Service<u16> for Discover<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::Future: Send + 'static,
    S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
{
    type Response = tonic::Response<
        futures::stream::BoxStream<'static, Result<Arc<ServerPolicy>, tonic::Status>>,
    >;
    type Error = tonic::Status;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, tonic::Status>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, port: u16) -> Self::Future {
        let req = api::PortSpec {
            port: port.into(),
            workload: self.workload.clone(),
        };
        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = client.watch_port(tonic::Request::new(req)).await?;
            Ok(rsp.map(|updates| {
                updates
                    .map(|up| match up {
                        Ok(p) => match to_policy(p) {
                            Ok(p) => Ok(p.into()),
                            Err(e) => Err(tonic::Status::invalid_argument(e.to_string())),
                        },
                        Err(e) => Err(e),
                    })
                    .boxed()
            }))
        })
    }
}

fn to_policy(proto: api::Server) -> Result<ServerPolicy> {
    let protocol = match proto.protocol {
        Some(api::ProxyProtocol { kind: Some(k) }) => match k {
            api::proxy_protocol::Kind::Detect(api::proxy_protocol::Detect { timeout }) => {
                Protocol::Detect {
                    timeout: match timeout {
                        Some(t) => t
                            .try_into()
                            .map_err(|t| format!("negative detect timeout: {:?}", t))?,
                        None => return Err("protocol missing detect timeout".into()),
                    },
                }
            }
            api::proxy_protocol::Kind::Http1(_) => Protocol::Http1,
            api::proxy_protocol::Kind::Http2(_) => Protocol::Http2,
            api::proxy_protocol::Kind::Grpc(_) => Protocol::Grpc,
            api::proxy_protocol::Kind::Opaque(_) => Protocol::Opaque,
            api::proxy_protocol::Kind::Tls(_) => Protocol::Tls,
        },
        _ => return Err("proxy protocol missing".into()),
    };

    let authorizations = proto
        .authorizations
        .into_iter()
        .map(
            |api::Authz {
                 labels,
                 authentication,
                 networks,
             }| {
                if networks.is_empty() {
                    return Err("networks missing".into());
                }
                let networks = networks
                    .into_iter()
                    .map(|api::Network { net, except }| {
                        let net = net.ok_or("network missing")?.try_into()?;
                        let except = except
                            .into_iter()
                            .map(|net| net.try_into())
                            .collect::<Result<Vec<IpNet>, _>>()?;
                        Ok(Network { net, except })
                    })
                    .collect::<Result<Vec<_>>>()?;

                let authn = match authentication.and_then(|api::Authn { permit }| permit) {
                    Some(api::authn::Permit::Unauthenticated(_)) => Authentication::Unauthenticated,
                    Some(api::authn::Permit::MeshTls(api::authn::PermitMeshTls { clients })) => {
                        match clients {
                            Some(api::authn::permit_mesh_tls::Clients::Unauthenticated(_)) => {
                                Authentication::TlsUnauthenticated
                            }
                            Some(api::authn::permit_mesh_tls::Clients::Identities(
                                api::authn::permit_mesh_tls::PermitClientIdentities {
                                    identities,
                                    suffixes,
                                },
                            )) => Authentication::TlsAuthenticated {
                                identities: identities
                                    .into_iter()
                                    .map(|api::Identity { name }| name)
                                    .collect(),
                                suffixes: suffixes
                                    .into_iter()
                                    .map(|api::IdentitySuffix { parts }| Suffix::from(parts))
                                    .collect(),
                            },
                            None => return Err("no clients permitted".into()),
                        }
                    }
                    authn => return Err(format!("no authentication provided: {:?}", authn).into()),
                };

                Ok(Authorization {
                    networks,
                    authentication: authn,
                    labels,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;

    Ok(ServerPolicy {
        protocol,
        authorizations,
        labels: proto.labels,
    })
}

// === impl GrpcRecover ===

impl Recover<tonic::Status> for GrpcRecover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, status: tonic::Status) -> Result<Self::Backoff, tonic::Status> {
        if status.code() == tonic::Code::InvalidArgument
            || status.code() == tonic::Code::FailedPrecondition
        {
            return Err(status);
        }

        tracing::trace!(%status, "Recovering");
        Ok(self.0.stream())
    }
}
