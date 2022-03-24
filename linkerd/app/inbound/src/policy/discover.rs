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
use std::{net::IpAddr, sync::Arc};

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
    type Response =
        tonic::Response<futures::stream::BoxStream<'static, Result<ServerPolicy, tonic::Status>>>;
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
                    .map(|up| match to_policy(up?) {
                        Ok(policy) => {
                            tracing::debug!(?policy);
                            Ok(policy)
                        }
                        Err(e) => Err(tonic::Status::invalid_argument(&*format!(
                            "received invalid policy: {}",
                            e
                        ))),
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

    let loopback = Authorization {
        kind: "default".into(),
        name: "localhost".into(),
        authentication: Authentication::Unauthenticated,
        networks: vec![
            Network {
                net: IpAddr::from([127, 0, 0, 1]).into(),
                except: vec![],
            },
            Network {
                net: IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]).into(),
                except: vec![],
            },
        ],
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

                let (kind, name) = kind_name(&labels, "serverauthorization")?;
                Ok(Authorization {
                    networks,
                    authentication: authn,
                    kind,
                    name,
                })
            },
        )
        .chain(Some(Ok(loopback)))
        .collect::<Result<Vec<_>>>()?;

    let (kind, name) = kind_name(&proto.labels, "server")?;
    Ok(ServerPolicy {
        protocol,
        authorizations,
        kind,
        name,
    })
}

fn kind_name(
    labels: &std::collections::HashMap<String, String>,
    default_kind: &str,
) -> Result<(Arc<str>, Arc<str>)> {
    let name = labels.get("name").ok_or("missing 'name' label")?.clone();
    let mut parts = name.splitn(2, ':');
    match (parts.next().unwrap(), parts.next()) {
        (kind, Some(name)) => Ok((kind.into(), name.into())),
        (name, None) => {
            let kind = labels
                .get("kind")
                .cloned()
                .unwrap_or_else(|| default_kind.to_string());
            Ok((kind.into(), name.into()))
        }
    }
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
