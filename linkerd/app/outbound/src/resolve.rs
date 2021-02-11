#![allow(warnings)]

use crate::target::{Concrete, Endpoint, EndpointFromMetadata};
use futures::{future, prelude::*, stream};
use linkerd_app_core::{
    discovery_rejected, is_discovery_rejected,
    proxy::{
        api_resolve::Metadata,
        core::{Resolve, ResolveService, Update},
        discover::{self, Buffer, FromResolve, MakeEndpoint},
        resolve::map_endpoint,
    },
    svc::{
        layer,
        stack::{Filter, Param, Predicate},
        NewService,
    },
    Addr, Error,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

type Stack<P, R, N> =
    Buffer<discover::Stack<N, map_endpoint::Resolve<EndpointFromMetadata, R>, Endpoint<P>>>;

pub fn layer<P, R, N>(
    resolve: R,
    watchdog: Duration,
) -> impl layer::Layer<N, Service = Stack<P, R, N>> + Clone
where
    P: Copy + Send + std::fmt::Debug,
    R: Resolve<Concrete<P>, Endpoint = Metadata> + Clone,
    R::Resolution: Send,
    R::Future: Send,
    N: NewService<Endpoint<P>>,
{
    const ENDPOINT_BUFFER_CAPACITY: usize = 1_000;

    let to_endpoint = EndpointFromMetadata;
    layer::mk(move |new_endpoint| {
        let endpoints = discover::resolve(
            new_endpoint,
            map_endpoint::Resolve::new(to_endpoint, resolve.clone()),
        );
        Buffer::new(ENDPOINT_BUFFER_CAPACITY, watchdog, endpoints)
    })
}
