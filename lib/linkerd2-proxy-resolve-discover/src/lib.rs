// use crate::core::resolve::{Resolution, Resolve, Update};
// use futures::{stream::FuturesUnordered, try_ready, Async, Future, Poll, Stream};
// use indexmap::IndexMap;
// use std::{fmt, net::SocketAddr};
// use tokio::sync::oneshot;
// use tower_discover::Change;
// use tracing::trace;

mod discover;
mod layer;

pub use self::discover::Discover;
pub use self::layer::{layer, DiscoverFuture, Layer, MakeDiscover};
