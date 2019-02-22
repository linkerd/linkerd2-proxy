use http;
use indexmap::IndexMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tower_retry::budget::Budget;

use proxy::http::{
    metrics::classify::{CanClassify, Classify, ClassifyEos, ClassifyResponse},
    profiles, retry, timeout,
};
use {Addr, NameAddr};

use super::classify;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub dst_addr: DstAddr,
    pub route: profiles::Route,
}

#[derive(Clone, Debug)]
pub struct Retry {
    budget: Arc<Budget>,
    response_classes: profiles::ResponseClasses,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DstAddr {
    addr: Addr,
    direction: Direction,
}

// === impl Route ===

impl CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl retry::CanRetry for Route {
    type Retry = Retry;

    fn can_retry(&self) -> Option<Self::Retry> {
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

// === impl Retry ===

impl retry::Retry for Retry {
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

// === impl DstAddr ===

impl AsRef<Addr> for DstAddr {
    fn as_ref(&self) -> &Addr {
        &self.addr
    }
}

impl DstAddr {
    pub fn outbound(addr: Addr) -> Self {
        DstAddr {
            addr,
            direction: Direction::Out,
        }
    }

    pub fn inbound(addr: Addr) -> Self {
        DstAddr {
            addr,
            direction: Direction::In,
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }
}

impl<'t> From<&'t DstAddr> for http::header::HeaderValue {
    fn from(a: &'t DstAddr) -> Self {
        http::header::HeaderValue::from_str(&format!("{}", a)).expect("addr must be a valid header")
    }
}

impl fmt::Display for DstAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.addr.fmt(f)
    }
}

impl profiles::CanGetDestination for DstAddr {
    fn get_destination(&self) -> Option<&NameAddr> {
        self.addr.name_addr()
    }
}

impl profiles::WithRoute for DstAddr {
    type Output = Route;

    fn with_route(self, route: profiles::Route) -> Self::Output {
        Route {
            dst_addr: self,
            route,
        }
    }
}

// === impl Route ===

impl Route {
    pub fn labels(&self) -> &Arc<IndexMap<String, String>> {
        self.route.labels()
    }
}
