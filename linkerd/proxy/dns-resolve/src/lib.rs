#![deny(warnings, rust_2018_idioms)]

use futures::{future, prelude::*, stream};
use linkerd2_addr::Addr;
use linkerd2_dns as dns;
use linkerd2_proxy_core::resolve::Update;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone)]
pub struct DnsResolve {
    dns: linkerd2_dns::Resolver,
}

impl DnsResolve {
    pub fn new(dns: dns::Resolver) -> Self {
        Self { dns }
    }
}

type UpdatesStream =
    Pin<Box<dyn Stream<Item = Result<Update<()>, dns::Error>> + Send + Sync + 'static>>;

impl<T: Into<Addr>> tower::Service<T> for DnsResolve {
    type Response = UpdatesStream;
    type Error = dns::Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        match target.into() {
            Addr::Name(na) if na.is_localhost() => {
                let sa = SocketAddr::from(([127, 0, 0, 1], na.port()));
                let eps = vec![(sa, ())];
                future::ok(Box::pin(stream::iter(Some(Ok(Update::Reset(eps))))))
            }
            Addr::Name(na) => {
                let resolve = self.dns.resolve_service_addrs(na.name().clone());
                let updates = resolve.map_ok(|addrs| {
                    let eps = addrs.into_iter().map(|a| (a, ())).collect();
                    Update::Reset(eps)
                });
                future::ok(Box::pin(updates))
            }
            Addr::Socket(sa) => {
                let eps = vec![(sa, ())];
                future::ok(Box::pin(stream::iter(Some(Ok(Update::Reset(eps))))))
            }
        }
    }
}
