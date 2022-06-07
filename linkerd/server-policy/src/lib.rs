#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod authz;
#[cfg(feature = "proto")]
mod proto;

pub use self::authz::{Authentication, Authorization};
use std::{borrow::Cow, hash::Hash, sync::Arc, time};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPolicy {
    pub protocol: Protocol,
    pub authorizations: Vec<Authorization>,
    pub meta: Arc<Meta>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Detect { timeout: time::Duration },
    Http1,
    Http2,
    Grpc,
    Opaque,
    Tls,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Meta {
    pub group: Cow<'static, str>,
    pub kind: Cow<'static, str>,
    pub name: Cow<'static, str>,
}
