#![allow(warnings)]

use crate::target::{Concrete, EndpointFromMetadata};
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

type ResolveStack<R> =
    map_endpoint::Resolve<EndpointFromMetadata, Filter<ResolveService<R>, ToAddr>>;

fn new_resolve<T, R>(resolve: R) -> ResolveStack<R>
where
    EndpointFromMetadata: map_endpoint::MapEndpoint<T, Metadata>,
    R: Resolve<Addr, Endpoint = Metadata>,
    R::Future: Send,
    R::Resolution: Send,
{
    map_endpoint::Resolve::new(
        EndpointFromMetadata,
        Filter::new(resolve.into_service(), ToAddr),
    )
}

type Stack<E, R, N> = Buffer<discover::Stack<N, ResolveStack<R>, E>>;

pub fn layer<T, E, R, N>(
    resolve: R,
    watchdog: Duration,
) -> impl layer::Layer<N, Service = Stack<E, R, N>> + Clone
where
    R: Resolve<Addr, Endpoint = Metadata> + Clone,
    R::Resolution: Send,
    R::Future: Send,
    EndpointFromMetadata: map_endpoint::MapEndpoint<T, Metadata, Out = E>,
    ResolveStack<R>: Resolve<T, Endpoint = E> + Clone,
    N: NewService<E>,
{
    const ENDPOINT_BUFFER_CAPACITY: usize = 1_000;

    let resolve = new_resolve(resolve);
    layer::mk(move |new_endpoint| {
        let endpoints = discover::resolve(new_endpoint, resolve.clone());
        Buffer::new(ENDPOINT_BUFFER_CAPACITY, watchdog, endpoints)
    })
}

#[derive(Clone, Debug)]
pub struct ToAddr;

// === impl ToAddr ===

impl<P> Predicate<Concrete<P>> for ToAddr {
    type Request = Addr;

    fn check(&mut self, target: Concrete<P>) -> Result<Addr, Error> {
        Ok(target.resolve)
    }
}
