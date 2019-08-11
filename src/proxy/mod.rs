//! Tools for building a transparent TCP/HTTP proxy.

pub mod accept;
pub mod buffer;
pub mod grpc;
pub mod http;
pub mod pending;
mod protocol;
pub mod reconnect;
pub mod resolve;
pub mod server;
mod tcp;

pub use self::accept::Accept;
pub use self::resolve::{Resolution, Resolve};
pub use self::server::{SpawnConnection, Server, Source};

type Error = Box<dyn std::error::Error + Send + Sync>;
