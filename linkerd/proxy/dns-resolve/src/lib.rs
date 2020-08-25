#![deny(warnings, rust_2018_idioms)]

use futures::{future, prelude::*, stream};
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns as dns;
use linkerd2_error::Error;
use linkerd2_proxy_core::resolve::Update;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tracing::debug;
use tracing_futures::Instrument;

/// A Resolver that attempts to lookup
#[derive(Clone)]
pub struct DnsResolve {
    dns: linkerd2_dns::Resolver,
}

impl DnsResolve {
    pub fn new(dns: dns::Resolver) -> Self {
        Self { dns }
    }
}

type UpdateStream = Pin<Box<dyn Stream<Item = Result<Update<()>, Error>> + Send + Sync + 'static>>;

impl<T: Into<Addr>> tower::Service<T> for DnsResolve {
    type Response = UpdateStream;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<UpdateStream, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        // If the target address is `localhost.`, skip DNS resolution and use
        // 127.0.0.1.
        let addr = match target.into() {
            Addr::Name(na) if na.is_localhost() => {
                SocketAddr::from(([127, 0, 0, 1], na.port())).into()
            }
            addr => addr,
        };

        match addr {
            Addr::Name(na) => Box::pin(resolution(self.dns.clone(), na).in_current_span()),
            Addr::Socket(sa) => {
                let eps = vec![(sa, ())];
                let updates: UpdateStream = Box::pin(stream::iter(Some(Ok(Update::Reset(eps)))));
                Box::pin(future::ok(updates))
            }
        }
    }
}

async fn resolution(dns: dns::Resolver, na: NameAddr) -> Result<UpdateStream, Error> {
    let (mut tx, rx) = mpsc::channel::<Result<Update<()>, Error>>(1);

    // Don't return a stream before the initial resolution completes. Then,
    // spawn a task to drive the continued resolution.
    let (addrs, expiry) = dns.resolve_addrs(na.name(), na.port()).await?;
    tokio::spawn(
        async move {
            let eps = addrs.into_iter().map(|a| (a, ())).collect();
            if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                debug!("Closed");
                return;
            }
            expiry.await;

            loop {
                match dns.resolve_addrs(na.name(), na.port()).await {
                    Ok((addrs, expiry)) => {
                        debug!(?addrs);
                        let eps = addrs.into_iter().map(|a| (a, ())).collect();
                        if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                            return;
                        }
                        expiry.await;
                    }
                    Err(e) => {
                        debug!(error = %e);
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        }
        .in_current_span(),
    );

    Ok(Box::pin(rx))
}
