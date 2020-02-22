use super::classify;
use crate::profiles;
use http;
use indexmap::IndexMap;
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_http_classify::CanClassify;
use linkerd2_proxy_http::{settings, timeout};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tower::retry::budget::Budget;

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
    dst_logical: Addr,
    dst_concrete: Addr,
    direction: Direction,
    pub http_settings: settings::Settings,
}

// === impl Route ===

impl CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl timeout::HasTimeout for Route {
    fn timeout(&self) -> Option<Duration> {
        self.route.timeout()
    }
}

// === impl DstAddr ===

impl AsRef<Addr> for DstAddr {
    fn as_ref(&self) -> &Addr {
        &self.dst_concrete
    }
}

impl DstAddr {
    pub fn outbound(addr: Addr, http_settings: settings::Settings) -> Self {
        DstAddr {
            dst_logical: addr.clone(),
            dst_concrete: addr,
            direction: Direction::Out,
            http_settings,
        }
    }

    pub fn inbound(addr: Addr, http_settings: settings::Settings) -> Self {
        DstAddr {
            dst_logical: addr.clone(),
            dst_concrete: addr,
            direction: Direction::In,
            http_settings,
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn dst_logical(&self) -> &Addr {
        &self.dst_logical
    }

    pub fn dst_concrete(&self) -> &Addr {
        &self.dst_concrete
    }
}

impl<'t> From<&'t DstAddr> for http::header::HeaderValue {
    fn from(a: &'t DstAddr) -> Self {
        http::header::HeaderValue::from_str(&format!("{}", a)).expect("addr must be a valid header")
    }
}

impl fmt::Display for DstAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.dst_concrete.fmt(f)
    }
}

impl profiles::CanGetDestination for DstAddr {
    fn get_destination(&self) -> Option<&NameAddr> {
        self.dst_logical.name_addr()
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

impl profiles::WithAddr for DstAddr {
    fn with_addr(mut self, addr: NameAddr) -> Self {
        self.dst_concrete = Addr::Name(addr);
        self
    }
}

// === impl Route ===

impl Route {
    pub fn labels(&self) -> &Arc<IndexMap<String, String>> {
        self.route.labels()
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.dst_addr.fmt(f)
    }
}
