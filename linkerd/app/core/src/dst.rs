use super::classify;
use crate::profiles;
use linkerd2_addr::Addr;
use linkerd2_http_classify::CanClassify;
// use linkerd2_proxy_http::timeout;
use std::fmt;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub target: Addr,
    pub route: profiles::Route,
    pub direction: super::metric_labels::Direction,
}

// === impl Route ===

impl CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

// impl timeout::HasTimeout for Route {
//     fn timeout(&self) -> Option<Duration> {
//         self.route.timeout()
//     }
// }

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.target.fmt(f)
    }
}
