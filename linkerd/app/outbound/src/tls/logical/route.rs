use super::super::Concrete;
use crate::{ParentRef, RouteRef};
use linkerd_app_core::{
    io,
    metrics::prom::EncodeLabelSetMut,
    svc,
    tls::ServerName,
    transport::metrics::tcp::{client::NewInstrumentConnection, TcpMetricsParams},
    Addr, Error,
};
use linkerd_distribute as distribute;
use linkerd_errno::{code::Code, Errno};
use linkerd_tls_route as tls_route;
use prometheus_client::encoding::*;
use std::{fmt::Debug, hash::Hash};

pub type TlsRouteMetrics = TcpMetricsParams<RouteLabels>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T> {
    pub(crate) route_ref: RouteRef,
    pub(crate) concrete: Concrete<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MatchedRoute<T> {
    pub(super) r#match: tls_route::RouteMatch,
    pub(super) params: Route<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route<T> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) parent_ref: ParentRef,
    pub(super) route_ref: RouteRef,
    pub(super) distribution: BackendDistribution<T>,
    pub(super) forbidden: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteLabels {
    parent: ParentRef,
    route: RouteRef,
    hostname: ServerName,
}

pub(crate) type BackendDistribution<T> = distribute::Distribution<Backend<T>>;
pub(crate) type NewDistribute<T, N> = distribute::NewDistribute<Backend<T>, (), N>;

/// Wraps errors with route metadata.
#[derive(Debug, thiserror::Error)]
#[error("route {}: {source}", route.0)]
struct RouteError {
    route: RouteRef,
    #[source]
    source: Error,
}

// === impl Backend ===

impl<T: Clone> Clone for Backend<T> {
    fn clone(&self) -> Self {
        Self {
            route_ref: self.route_ref.clone(),
            concrete: self.concrete.clone(),
        }
    }
}

// === impl MatchedRoute ===

impl<T> MatchedRoute<T>
where
    // Parent target.
    T: Debug + Eq + Hash,
    T: Clone + Send + Sync + 'static,
    T: svc::Param<ServerName>,
{
    /// Builds a route stack that applies policy filters to requests and
    /// distributes requests over each route's backends. These [`Concrete`]
    /// backends are expected to be cached/shared by the inner stack.
    pub(crate) fn layer<N, I, NSvc>(
        metrics: TlsRouteMetrics,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneTcp<Self, I>> + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Inner stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
    {
        svc::layer::mk(move |inner| {
            svc::stack(inner)
                .push_map_target(|t| t)
                .push_map_target(|b: Backend<T>| b.concrete)
                .lift_new()
                .push(NewDistribute::layer())
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer())
                .push_filter(|rt: Self| -> Result<_, Error> {
                    if rt.params.forbidden {
                        Err(Errno::from(Code::ECONNREFUSED).into())
                    } else {
                        Ok(rt)
                    }
                })
                .push(svc::NewMapErr::layer_with(|rt: &Self| {
                    let route = rt.params.route_ref.clone();
                    move |source| RouteError {
                        route: route.clone(),
                        source,
                    }
                }))
                .push(NewInstrumentConnection::layer(metrics.clone()))
                .arc_new_clone_tcp()
                .into_inner()
        })
    }
}

impl<T: Clone> svc::Param<BackendDistribution<T>> for MatchedRoute<T> {
    fn param(&self) -> BackendDistribution<T> {
        self.params.distribution.clone()
    }
}

impl<T> svc::Param<RouteLabels> for MatchedRoute<T>
where
    T: Eq + Hash + Clone + Debug,
    T: svc::Param<ServerName>,
{
    fn param(&self) -> RouteLabels {
        RouteLabels {
            route: self.params.route_ref.clone(),
            parent: self.params.parent_ref.clone(),
            hostname: self.params.parent.param(),
        }
    }
}

// === impl RouteLabels ===

impl EncodeLabelSetMut for RouteLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self {
            parent,
            route,
            hostname,
        } = self;
        parent.encode_label_set(enc)?;
        route.encode_label_set(enc)?;
        ("hostname", hostname.to_string()).encode(enc.encode_label())?;
        Ok(())
    }
}

impl EncodeLabelSet for RouteLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
