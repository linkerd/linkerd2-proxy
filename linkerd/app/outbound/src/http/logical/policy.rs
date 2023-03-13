use super::{Concrete, LogicalAddr};
use linkerd_app_core::{proxy::http, svc, Addr, Error, Infallible};
use std::{fmt::Debug, hash::Hash};

mod route;
mod router;
#[cfg(test)]
mod tests;

pub use self::{
    route::errors,
    router::{GrpcParams, HttpParams},
};
pub use linkerd_proxy_client_policy::ClientPolicy;

/// HTTP or gRPC policy route parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Params {
    Http(router::HttpParams),
    Grpc(router::GrpcParams),
}

/// A stack module configured by `Params` and some `T`-typed parent target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Policy<T: Clone + Debug + Eq + Hash> {
    Http(router::Http<T>),
    Grpc(router::Grpc<T>),
}

// === impl Params ===

impl Params {
    pub fn addr(&self) -> &Addr {
        match self {
            Params::Http(router::Params { ref addr, .. })
            | Params::Grpc(router::Params { ref addr, .. }) => addr,
        }
    }
}

// === impl Policy ===

impl<T> Policy<T>
where
    // Parent target type.
    T: Debug + Eq + Hash,
    T: Clone + Send + Sync + 'static,
{
    /// Wraps an HTTP `NewService` with HTTP or gRPC policy routing layers.
    pub(super) fn layer<N, S>() -> impl svc::Layer<
        N,
        Service = svc::ArcNewService<
            Self,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    > + Clone
    where
        // Inner stack.
        N: svc::NewService<Concrete<T>, Service = S>,
        N: Clone + Send + Sync + 'static,
        S: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
    {
        svc::layer::mk(|inner: N| {
            let http = svc::stack(inner.clone()).push(router::Http::layer());
            let grpc = svc::stack(inner).push(router::Grpc::layer());

            http.push_switch(
                |pp: Policy<T>| {
                    Ok::<_, Infallible>(match pp {
                        Self::Http(http) => svc::Either::A(http),
                        Self::Grpc(grpc) => svc::Either::B(grpc),
                    })
                },
                grpc.into_inner(),
            )
            .push(svc::ArcNewService::layer())
            .into_inner()
        })
    }
}

impl<T> From<(Params, T)> for Policy<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((pr, parent): (Params, T)) -> Self {
        match pr {
            Params::Http(http) => Policy::Http(router::Http::from((http, parent))),
            Params::Grpc(grpc) => Policy::Grpc(router::Grpc::from((grpc, parent))),
        }
    }
}

impl<T> svc::Param<LogicalAddr> for Policy<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> LogicalAddr {
        match self {
            Policy::Http(p) => p.param(),
            Policy::Grpc(p) => p.param(),
        }
    }
}
