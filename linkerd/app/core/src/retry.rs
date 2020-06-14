use super::classify;
use super::dst::Route;
// use super::handle_time;
use super::http_metrics::retries::Handle;
use super::transport::tls;
use super::HttpRouteRetry;
use crate::profiles;
use futures::future;
use hyper::body::HttpBody;
use linkerd2_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd2_retry::NewRetryLayer;
use std::marker::PhantomData;
use std::sync::Arc;
use tower::retry::budget::Budget;

pub fn layer(metrics: HttpRouteRetry) -> NewRetryLayer<NewRetry> {
    NewRetryLayer::new(NewRetry::new(metrics))
}

pub trait CloneRequest<Req> {
    fn clone_request(req: &Req) -> Option<Req>;
}

#[derive(Clone, Debug)]
pub struct NewRetry<C = ()> {
    metrics: HttpRouteRetry,
    _clone_request: PhantomData<C>,
}

pub struct Retry<C = ()> {
    metrics: Handle,
    budget: Arc<Budget>,
    response_classes: profiles::ResponseClasses,
    _clone_request: PhantomData<C>,
}

impl NewRetry {
    pub fn new(metrics: super::HttpRouteRetry) -> Self {
        Self {
            metrics,
            _clone_request: PhantomData,
        }
    }

    pub fn clone_requests_via<C>(self) -> NewRetry<C> {
        NewRetry {
            metrics: self.metrics,
            _clone_request: PhantomData,
        }
    }
}

impl<C> linkerd2_retry::NewPolicy<Route> for NewRetry<C> {
    type Policy = Retry<C>;

    fn new_policy(&self, route: &Route) -> Option<Self::Policy> {
        let retries = route.route.retries().cloned()?;

        let metrics = self.metrics.get_handle(route.clone());
        Some(Retry {
            metrics,
            budget: retries.budget().clone(),
            response_classes: route.route.response_classes().clone(),
            _clone_request: self._clone_request,
        })
    }
}

impl<C, A, B, E> linkerd2_retry::Policy<http::Request<A>, http::Response<B>, E> for Retry<C>
where
    C: CloneRequest<http::Request<A>>,
{
    type Future = future::Ready<Self>;

    fn retry(
        &self,
        req: &http::Request<A>,
        result: Result<&http::Response<B>, &E>,
    ) -> Option<Self::Future> {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => classify::Request::from(self.response_classes.clone())
                .classify(req)
                .start(rsp)
                .eos(None)
                .is_failure(),
        };

        if !retryable {
            self.budget.deposit();
            return None;
        }

        let withdrew = self.budget.withdraw().is_ok();
        self.metrics.incr_retryable(withdrew);
        if !withdrew {
            return None;
        }

        Some(future::ready(self.clone()))
    }

    fn clone_request(&self, req: &http::Request<A>) -> Option<http::Request<A>> {
        C::clone_request(req)
    }
}

impl<C> Clone for Retry<C> {
    fn clone(&self) -> Self {
        Self {
            metrics: self.metrics.clone(),
            budget: self.budget.clone(),
            response_classes: self.response_classes.clone(),
            _clone_request: self._clone_request,
        }
    }
}

impl<B: Default + HttpBody> CloneRequest<http::Request<B>> for () {
    fn clone_request(req: &http::Request<B>) -> Option<http::Request<B>> {
        if !req.body().is_end_stream() {
            return None;
        }

        let mut clone = http::Request::new(B::default());
        *clone.method_mut() = req.method().clone();
        *clone.uri_mut() = req.uri().clone();
        *clone.headers_mut() = req.headers().clone();
        *clone.version_mut() = req.version();

        if let Some(ext) = req.extensions().get::<tls::accept::Meta>() {
            clone.extensions_mut().insert(ext.clone());
        }

        // // Count retries toward the request's total handle time.
        // if let Some(ext) = req.extensions().get::<handle_time::Tracker>() {
        //     clone.extensions_mut().insert(ext.clone());
        // }

        Some(clone)
    }
}
