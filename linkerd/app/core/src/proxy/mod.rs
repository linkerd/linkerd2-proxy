//! Tools for building a transparent TCP/HTTP proxy.

pub use linkerd2_proxy_api_resolve as api_resolve;
pub use linkerd2_proxy_core as core;
pub use linkerd2_proxy_detect as detect;
pub use linkerd2_proxy_discover as discover;
pub use linkerd2_proxy_http::{self as http, grpc};
pub use linkerd2_proxy_resolve as resolve;
pub use linkerd2_proxy_tap as tap;

pub mod buffer;
pub mod pending;
pub mod server;
mod tcp;

pub use self::server::Server;
