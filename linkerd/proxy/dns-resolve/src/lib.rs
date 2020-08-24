#![deny(warnings, rust_2018_idioms)]

use async_stream::try_stream;
use futures::{future, prelude::*};
use linkerd2_addr::Addr;
use linkerd2_dns as dns;
use linkerd2_error::Error;
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

type UpdatesStream = Pin<Box<dyn Stream<Item = Result<Update<()>, Error>> + Send + Sync + 'static>>;

impl<T: Into<Addr>> tower::Service<T> for DnsResolve {
    type Response = UpdatesStream;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        match target.into() {
            Addr::Name(na) if na.is_localhost() => {
                let sa = SocketAddr::from(([127, 0, 0, 1], na.port()));
                let stream = try_stream! {
                    yield Update::Reset(vec![(sa, ())]);
                };
                future::ok(Box::pin(stream))
            }
            Addr::Name(na) => {
                let resolve = self.dns.resolve_service_ips(na.name());
                let stream = try_stream! {
                    tokio::pin!(resolve);
                    while let Some(addrs) = resolve.next().await {
                        let addrs = addrs?;
                        yield Update::Reset(addrs.into_iter().map(|a| (a, ())).collect());
                    }
                };
                future::ok(Box::pin(stream))
            }
            Addr::Socket(sa) => {
                let stream = try_stream! {
                    yield Update::Reset(vec![(sa, ())]);
                };
                future::ok(Box::pin(stream))
            }
        }
    }
}
