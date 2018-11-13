use http;
use std::fmt;

use proxy::http::{metrics::classify::CanClassify, profiles};
use {Addr, NameAddr};

use super::classify;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug)]
pub struct Route {
    pub dst_addr: DstAddr,
    pub route: profiles::Route,
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

// === impl DstAddr ===

impl AsRef<Addr> for DstAddr {
    fn as_ref(&self) -> &Addr {
        &self.addr
    }
}

impl DstAddr {
    pub fn outbound(addr: Addr) -> Self {
        DstAddr { addr, direction: Direction::Out }
    }

    pub fn inbound(addr: Addr) -> Self {
        DstAddr { addr, direction: Direction::In }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }
}

impl<'t> From<&'t DstAddr> for http::header::HeaderValue {
    fn from(a: &'t DstAddr) -> Self {
        http::header::HeaderValue::from_str(&format!("{}", a))
            .expect("addr must be a valid header")
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
