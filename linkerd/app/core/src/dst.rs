use super::classify;
use http;
use linkerd2_addr::Addr;
use linkerd2_proxy_http::{
    metrics::classify::{CanClassify, Classify, ClassifyEos, ClassifyResponse},
    profiles, retry, timeout,
};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub target: Addr,
    pub route: profiles::Route,
    pub direction: super::metric_labels::Direction,
}

#[derive(Clone, Debug)]
pub struct Retry {
    budget: Arc<retry::Budget>,
    response_classes: profiles::ResponseClasses,
}

// === impl Route ===

impl CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl retry::HasPolicy for Route {
    type Policy = Retry;

    fn retry_policy(&self) -> Option<Self::Policy> {
        self.route.retries().map(|retries| Retry {
            budget: retries.budget().clone(),
            response_classes: self.route.response_classes().clone(),
        })
    }
}

impl timeout::HasTimeout for Route {
    fn timeout(&self) -> Option<Duration> {
        self.route.timeout()
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.target.fmt(f)
    }
}

// === impl Retry ===

impl retry::Policy for Retry {
    fn retry<B1, B2>(
        &self,
        req: &http::Request<B1>,
        res: &http::Response<B2>,
    ) -> Result<(), retry::NoRetry> {
        let class = classify::Request::from(self.response_classes.clone())
            .classify(req)
            .start(res)
            .eos(None);

        if class.is_failure() {
            return self
                .budget
                .withdraw()
                .map_err(|_overdrawn| retry::NoRetry::Budget);
        }

        self.budget.deposit();
        Err(retry::NoRetry::Success)
    }

    fn clone_request<B: retry::TryClone>(
        &self,
        req: &http::Request<B>,
    ) -> Option<http::Request<B>> {
        retry::TryClone::try_clone(req).map(|mut clone| {
            if let Some(ext) = req.extensions().get::<classify::Response>() {
                clone.extensions_mut().insert(ext.clone());
            }
            clone
        })
    }
}
