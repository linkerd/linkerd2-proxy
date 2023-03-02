use super::super::Concrete;
use linkerd_app_core::{proxy::http, svc, Addr, Error, Result};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

pub(crate) mod filters;

pub use self::filters::errors;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Matched<M, P> {
    pub(super) r#match: http_route::RouteMatch<M>,
    pub(super) params: P,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route<T, F> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) meta: Arc<policy::Meta>,
    pub(super) filters: Arc<[F]>,
    pub(super) distribution: Distribution<T, F>,
}

pub(crate) type MatchedRoute<T, M, F> = Matched<M, Route<T, F>>;
pub(crate) type HttpMatchedRoute<T> =
    MatchedRoute<T, http_route::http::r#match::RequestMatch, policy::http::Filter>;
pub(crate) type GrpcMatchedRoute<T> =
    MatchedRoute<T, http_route::grpc::r#match::RouteMatch, policy::grpc::Filter>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T, F> {
    pub(super) concrete: Concrete<T>,
    pub(super) filters: Arc<[F]>,
}

pub(crate) type MatchedBackend<T, M, F> = Matched<M, Backend<T, F>>;
pub(crate) type HttpMatchedBackend<T> =
    MatchedBackend<T, http_route::http::r#match::RequestMatch, policy::http::Filter>;
pub(crate) type GrpcMatchedBackend<T> =
    MatchedBackend<T, http_route::grpc::r#match::RouteMatch, policy::grpc::Filter>;

pub(crate) type Distribution<T, F> = distribute::Distribution<Backend<T, F>>;
pub(crate) type NewDistribute<T, F, N> = distribute::NewDistribute<Backend<T, F>, (), N>;

// === impl MatchedRoute ===

impl<T, M, F> MatchedRoute<T, M, F>
where
    // Parent target.
    T: Clone + Send + Sync + 'static,
    // Match summary
    M: Clone + Send + Sync + 'static,
    // Request filter.
    F: Clone + Send + Sync + 'static,
    Self: filters::Apply,
{
    pub(crate) fn layer<N, S>() -> impl svc::Layer<
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
        T: Debug + Eq + Hash,
        F: Debug + Eq + Hash,
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
        MatchedBackend<T, M, F>: filters::Apply,
    {
        svc::layer::mk(|inner| {
            svc::stack(inner)
                .push(MatchedBackend::layer())
                .lift_new_with_target()
                .push(NewDistribute::layer())
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer())
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

impl<T: Clone, M, F> svc::Param<Distribution<T, F>> for MatchedRoute<T, M, F> {
    fn param(&self) -> Distribution<T, F> {
        self.params.distribution.clone()
    }
}

impl<T> filters::Apply for HttpMatchedRoute<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_http(&self.r#match, &self.params.filters, req)
    }
}

impl<T> filters::Apply for GrpcMatchedRoute<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_grpc(&self.r#match, &self.params.filters, req)
    }
}

// === impl Backend ===

impl<T: Clone, F> Clone for Backend<T, F> {
    fn clone(&self) -> Self {
        Self {
            filters: self.filters.clone(),
            concrete: self.concrete.clone(),
        }
    }
}

// === impl MatchedBackend ===

impl<M, T, F> From<(Backend<T, F>, MatchedRoute<T, M, F>)> for MatchedBackend<T, M, F> {
    fn from((params, route): (Backend<T, F>, MatchedRoute<T, M, F>)) -> Self {
        Matched {
            r#match: route.r#match,
            params,
        }
    }
}

impl<T, M, F> MatchedBackend<T, M, F> {
    pub(crate) fn layer<N, S>() -> impl svc::Layer<
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
        T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        M: Clone + Send + Sync + 'static,
        F: Clone + Send + Sync + 'static,
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
        Self: filters::Apply,
    {
        svc::layer::mk(|inner| {
            svc::stack(inner)
                .push_map_target(
                    |Matched {
                         params: Backend { concrete, .. },
                         ..
                     }| concrete,
                )
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

impl<T> filters::Apply for HttpMatchedBackend<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_http(&self.r#match, &self.params.filters, req)
    }
}

impl<T> filters::Apply for GrpcMatchedBackend<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_grpc(&self.r#match, &self.params.filters, req)
    }
}
