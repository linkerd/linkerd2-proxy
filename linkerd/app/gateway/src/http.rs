use super::Gateway;
use inbound::{policy::Permitted, GatewayAddr, GatewayDomainInvalid};
use linkerd_app_core::{
    metrics::ServerLabel,
    profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
    },
    svc, tls,
    transport::addrs::*,
    Error,
};
use linkerd_app_inbound as inbound;
use linkerd_app_outbound as outbound;
use linkerd_proxy_client_policy as policy;
use std::fmt::Debug;
use tokio::sync::watch;

mod gateway;
#[cfg(test)]
mod tests;

pub(crate) use self::gateway::NewHttpGateway;

/// Target for outbound HTTP gateway stacks.
#[derive(Clone, Debug)]
pub struct Target<T = ()> {
    addr: GatewayAddr,
    routes: watch::Receiver<outbound::http::Routes>,
    version: http::Variant,
    parent: T,
}

/// Implements `svc::router::SelectRoute` for outbound HTTP requests. An
/// `OutboundHttp` target is returned for each request using the request's HTTP
/// version.
///
/// The request's HTTP version may not match the target's original HTTP version
/// when proxies use HTTP/2 to transport HTTP/1 requests.
#[derive(Clone, Debug)]
struct ByRequestVersion<T>(Target<T>);

impl Gateway {
    /// Wrap the provided outbound HTTP client with the inbound HTTP server,
    /// inbound authorization, tagged-transport gateway routing, and the
    /// outbound router.
    pub fn http<T, R>(
        &self,
        inner: svc::ArcNewHttp<
            outbound::http::concrete::Endpoint<
                outbound::http::logical::Concrete<outbound::http::Http<Target>>,
            >,
        >,
        resolve: R,
    ) -> svc::Stack<
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
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<ServerLabel>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<Option<watch::Receiver<profiles::Profile>>>,
        T: svc::Param<outbound::policy::Receiver>,
        T: svc::Param<http::Variant>,
        T: svc::Param<http::normalize_uri::DefaultAuthority>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
    {
        let http = self
            .outbound
            .clone()
            .with_stack(inner)
            .push_http_cached(resolve)
            .into_stack()
            // Discard `T` and its associated client-specific metadata.
            .push_map_target(Target::discard_parent)
            .push(svc::ArcNewService::layer())
            // Add headers to prevent loops.
            .push(NewHttpGateway::layer(
                self.inbound.identity().local_id().clone(),
            ))
            .push_on_service(svc::LoadShed::layer())
            .lift_new()
            .push(svc::ArcNewService::layer())
            // After protocol-downgrade, we need to build an inner stack for
            // each request-level HTTP version.
            .push(svc::NewOneshotRoute::layer_via(|t: &Target<T>| {
                ByRequestVersion(t.clone())
            }))
            // Only permit gateway traffic to endpoints for which we have
            // discovery information.
            .push_filter(spawn_routes)
            .push(svc::ArcNewService::layer())
            // Authorize requests to the gateway.
            .push(self.inbound.authorize_http())
            .arc_new_clone_http();

        self.inbound
            .clone()
            .with_stack(http.into_inner())
            // Teminates HTTP connections.
            // XXX Sets an identity header -- this should probably not be done
            // in the gateway, though the value will be stripped by meshed
            // servers.
            .push_http_server()
            .into_stack()
            .arc_new_clone_http()
    }
}

fn spawn_routes<T>(permitted: Permitted<T>) -> Result<Target<T>, GatewayDomainInvalid>
where
    T: Clone
        + svc::Param<GatewayAddr>
        + svc::Param<http::Variant>
        + svc::Param<Option<watch::Receiver<profiles::Profile>>>
        + svc::Param<outbound::policy::Receiver>,
{
    let parent = permitted.into_target();
    let addr: GatewayAddr = parent.param();
    let version: http::Variant = parent.param();

    if let Some(mut profile) =
        svc::Param::<Option<watch::Receiver<profiles::Profile>>>::param(&parent)
    {
        if let Some(laddr) = outbound::http::profile::should_override_policy(&profile) {
            tracing::debug!("Using ServiceProfile");
            let init = mk_profile_routes(laddr.clone(), &profile.borrow_and_update());
            let routes =
                outbound::http::spawn_routes(profile, init, move |profile: &profiles::Profile| {
                    Some(mk_profile_routes(laddr.clone(), profile))
                });
            return Ok(Target {
                routes,
                addr,
                version,
                parent,
            });
        }
    }

    tracing::debug!("Using ClientPolicy routes");
    let mut policy: outbound::policy::Receiver = parent.param();
    let init = mk_policy_routes(addr.clone(), version, &policy.borrow_and_update())
        .ok_or(GatewayDomainInvalid)?;
    let routes = outbound::http::spawn_routes(policy, init, {
        let addr = addr.clone();
        move |policy: &policy::ClientPolicy| mk_policy_routes(addr.clone(), version, policy)
    });

    Ok(Target {
        routes,
        addr,
        version,
        parent,
    })
}

fn mk_policy_routes(
    GatewayAddr(addr): GatewayAddr,
    version: http::Variant,
    policy: &policy::ClientPolicy,
) -> Option<outbound::http::Routes> {
    let parent_ref = outbound::ParentRef::from(policy.parent.clone());

    let (routes, failure_accrual) = match policy.protocol {
        policy::Protocol::Detect {
            ref http1,
            ref http2,
            ..
        } => match version {
            http::Variant::Http1 => (http1.routes.clone(), http1.failure_accrual),
            http::Variant::H2 => (http2.routes.clone(), http2.failure_accrual),
        },
        policy::Protocol::Http1(policy::http::Http1 {
            ref routes,
            failure_accrual,
        }) => (routes.clone(), failure_accrual),
        policy::Protocol::Http2(policy::http::Http2 {
            ref routes,
            failure_accrual,
        }) => (routes.clone(), failure_accrual),
        policy::Protocol::Grpc(policy::grpc::Grpc {
            ref routes,
            failure_accrual,
        }) => {
            return Some(outbound::http::Routes::Policy(
                outbound::http::policy::Params::Grpc(outbound::http::policy::GrpcParams {
                    addr: addr.into(),
                    meta: parent_ref,
                    backends: policy.backends.clone(),
                    routes: routes.clone(),
                    failure_accrual,
                }),
            ))
        }
        policy::Protocol::Opaque(_) | policy::Protocol::Tls(_) => {
            tracing::info!("Ignoring a discovery update that changed a route from HTTP to opaque");
            return None;
        }
    };

    Some(outbound::http::Routes::Policy(
        outbound::http::policy::Params::Http(outbound::http::policy::HttpParams {
            addr: addr.into(),
            meta: parent_ref,
            routes,
            backends: policy.backends.clone(),
            failure_accrual,
        }),
    ))
}

fn mk_profile_routes(
    addr: profiles::LogicalAddr,
    profile: &profiles::Profile,
) -> outbound::http::Routes {
    outbound::http::Routes::Profile(outbound::http::profile::Routes {
        addr,
        routes: profile.http_routes.clone(),
        targets: profile.targets.clone(),
    })
}

// === impl ByRequestVersion ===

impl<B, T: Clone> svc::router::SelectRoute<http::Request<B>> for ByRequestVersion<T> {
    type Key = Target<T>;
    type Error = http::UnsupportedVariant;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let mut t = self.0.clone();
        t.version = req.version().try_into()?;
        Ok(t)
    }
}

// === impl Target ===

impl<T> Target<T> {
    fn discard_parent(self) -> Target {
        Target {
            addr: self.addr,
            routes: self.routes,
            version: self.version,
            parent: (),
        }
    }
}

impl<T> svc::Param<GatewayAddr> for Target<T> {
    fn param(&self) -> GatewayAddr {
        self.addr.clone()
    }
}

impl<T> svc::Param<http::Variant> for Target<T> {
    fn param(&self) -> http::Variant {
        self.version
    }
}

impl<T> svc::Param<tls::ClientId> for Target<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        self.parent.param()
    }
}

impl<T> svc::Param<watch::Receiver<outbound::http::Routes>> for Target<T> {
    fn param(&self) -> watch::Receiver<outbound::http::Routes> {
        self.routes.clone()
    }
}

impl<T> PartialEq for Target<T> {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr && self.version == other.version
    }
}

impl<T> Eq for Target<T> {}

impl<T: std::hash::Hash> std::hash::Hash for Target<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.version.hash(state);
        self.parent.hash(state);
    }
}
