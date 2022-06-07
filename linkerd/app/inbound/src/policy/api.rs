use futures::prelude::*;
use linkerd2_proxy_api::inbound::{
    self as api, inbound_server_policies_client::InboundServerPoliciesClient as Client,
};
use linkerd_app_core::{
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::http,
    svc::Service,
    Error, IpNet, Recover, Result,
};
use linkerd_server_policy::{
    Authentication, Authorization, Meta, Network, Protocol, ServerPolicy, Suffix,
};
use linkerd_tonic_watch::StreamWatch;
use std::{borrow::Cow, net::IpAddr, sync::Arc};

#[derive(Clone, Debug)]
pub(super) struct Api<S> {
    workload: String,
    client: Client<S>,
}

#[derive(Clone)]
pub(super) struct GrpcRecover(ExponentialBackoff);

pub(super) type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

impl<S> Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    pub(super) fn new(workload: String, client: S) -> Self {
        Self {
            workload,
            client: Client::new(client),
        }
    }

    pub(super) fn into_watch(self, backoff: ExponentialBackoff) -> Watch<S> {
        StreamWatch::new(GrpcRecover(backoff), self)
    }
}

impl<S> Service<u16> for Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    S::Future: Send + 'static,
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
        meta: Arc::new(Meta {
            group: "default".into(),
            kind: "default".into(),
            name: "localhost".into(),
        }),
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

                let meta = mk_meta(&labels, "serverauthorization")?;
                Ok(Authorization {
                    networks,
                    authentication: authn,
                    meta,
                })
            },
        )
        .chain(Some(Ok(loopback)))
        .collect::<Result<Vec<_>>>()?;

    let meta = mk_meta(&proto.labels, "server")?;
    Ok(ServerPolicy {
        protocol,
        authorizations,
        meta,
    })
}

fn mk_meta(
    labels: &std::collections::HashMap<String, String>,
    default_kind: &'static str,
) -> Result<Arc<Meta>> {
    let group = labels
        .get("group")
        .cloned()
        .map(Cow::Owned)
        // If no group is specified, we leave it blank. This is to avoid setting
        // a group when using synthetic kinds like "default".
        .unwrap_or(Cow::Borrowed(""));

    let name = labels.get("name").ok_or("missing 'name' label")?.clone();
    if let Some(kind) = labels.get("kind").cloned() {
        return Ok(Arc::new(Meta {
            group,
            kind: kind.into(),
            name: name.into(),
        }));
    }

    // Older control plane versions don't set the kind label and, instead, may
    // encode kinds in the name like `default:deny`.
    let mut parts = name.splitn(2, ':');
    let meta = match (parts.next().unwrap().to_owned(), parts.next()) {
        (kind, Some(name)) => Meta {
            group,
            kind: kind.into(),
            name: name.to_owned().into(),
        },
        (name, None) => Meta {
            group,
            kind: Cow::Borrowed(default_kind),
            name: name.into(),
        },
    };

    Ok(Arc::new(meta))
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
