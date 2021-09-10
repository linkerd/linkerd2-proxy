use super::classify;
use crate::profiles;
use linkerd_http_classify::CanClassify;
use linkerd_proxy_http as http;
use linkerd_stack::Param;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub addr: profiles::LogicalAddr,
    pub route: profiles::http::Route,
    pub direction: super::metrics::Direction,
}

// === impl Route ===

impl CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl Param<http::ResponseTimeout> for Route {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.route.timeout())
    }
}
