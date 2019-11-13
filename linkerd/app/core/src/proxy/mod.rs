//! Tools for building a transparent TCP/HTTP proxy.

pub use linkerd2_fallback as fallback;
pub use linkerd2_proxy_api_resolve as api_resolve;
pub use linkerd2_proxy_core as core;
pub use linkerd2_proxy_detect as detect;
pub use linkerd2_proxy_discover as discover;
pub use linkerd2_proxy_http::{self as http, grpc};
pub use linkerd2_proxy_identity as identity;
pub use linkerd2_proxy_resolve as resolve;
pub use linkerd2_proxy_tap as tap;
pub use linkerd2_proxy_tcp as tcp;

pub mod buffer;
pub mod pending;
pub mod server;

pub use self::server::Server;
