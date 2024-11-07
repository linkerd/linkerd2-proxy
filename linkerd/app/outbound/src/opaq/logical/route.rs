use super::{super::Concrete, Logical};
use crate::{ParentRef, RouteRef};
use linkerd_app_core::{
    io,
    metrics::prom::EncodeLabelSetMut,
    svc,
    transport::metrics::tcp::{client::NewInstrumentConnection, TcpMetricsParams},
    Addr, Error,
};
use linkerd_distribute as distribute;
use linkerd_errno::{code::Code, Errno};
use prometheus_client::encoding::*;
use std::{fmt::Debug, hash::Hash};

pub type TcpRouteMetrics = TcpMetricsParams<RouteLabels>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T> {
    pub(crate) route_ref: RouteRef,
    pub(crate) concrete: Concrete<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MatchedRoute<T> {
    pub(super) params: Route<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route<T> {
    pub(super) parent: T,
    pub(super) logical: Logical,
    pub(super) route_ref: RouteRef,
    pub(super) distribution: BackendDistribution<T>,
    pub(super) forbidden: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteLabels {
    parent: ParentRef,
    route: RouteRef,
    addr: Addr,
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
{
    /// Builds a route stack that applies policy filters to requests and
    /// distributes requests over each route's backends. These [`Concrete`]
    /// backends are expected to be cached/shared by the inner stack.
    pub(crate) fn layer<N, I, NSvc>(
        metrics: TcpRouteMetrics,
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
{
    fn param(&self) -> RouteLabels {
        RouteLabels {
            route: self.params.route_ref.clone(),
            parent: self.params.logical.meta.clone(),
            addr: self.params.logical.addr.clone(),
        }
    }
}

// === impl RouteLabels ===

impl EncodeLabelSetMut for RouteLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self {
            parent,
            route,
            addr,
        } = self;

        parent.encode_label_set(enc)?;
        route.encode_label_set(enc)?;

        (
            "target_ip",
            match addr {
                Addr::Socket(ref a) => Some(a.ip().to_string()),
                Addr::Name(_) => None,
            },
        )
            .encode(enc.encode_label())?;

        ("target_port", addr.port()).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for RouteLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
