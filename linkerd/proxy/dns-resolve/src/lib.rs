#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use linkerd_addr::{Addr, NameAddr};
use linkerd_dns as dns;
use linkerd_error::Error;
use linkerd_proxy_core::resolve::Update;
use linkerd_stack::Param;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tracing::instrument::Instrument;
use tracing::{debug, trace};

/// A Resolver that attempts to lookup targets via DNS.
///
/// SRV records are checked first, A records are used as a fallback.
#[derive(Clone)]
pub struct DnsResolve {
    dns: linkerd_dns::Resolver,
}

impl DnsResolve {
    pub fn new(dns: dns::Resolver) -> Self {
        Self { dns }
    }
}

type UpdateStream = Pin<Box<dyn Stream<Item = Result<Update<()>, Error>> + Send + Sync + 'static>>;

impl<T: Param<Addr>> tower::Service<T> for DnsResolve {
    type Response = UpdateStream;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<UpdateStream, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        // If the target address is `localhost.`, skip DNS resolution and use
        // 127.0.0.1.
        let addr = match target.param() {
            Addr::Name(na) if na.is_localhost() => {
                SocketAddr::from(([127, 0, 0, 1], na.port())).into()
            }
            addr => addr,
        };

        match addr {
            Addr::Name(na) => Box::pin(resolution(self.dns.clone(), na).in_current_span()),
            Addr::Socket(sa) => {
                let eps = vec![(sa, ())];
                let updates: UpdateStream =
                    Box::pin(stream::iter(Some(Ok(Update::Reset(eps)))).chain(stream::pending()));
                Box::pin(future::ok(updates))
            }
        }
    }
}

async fn resolution(dns: dns::Resolver, na: NameAddr) -> Result<UpdateStream, Error> {
    use tokio_stream::wrappers::ReceiverStream;

    // Don't return a stream before the initial resolution completes. Then,
    // spawn a task to drive the continued resolution.
    //
    // Note: this can't be an async_stream, due to pinniness.
    let (addrs, expiry) = dns.resolve_addrs(na.name().as_ref(), na.port()).await?;
    debug!(?addrs, name = %na);
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(
        async move {
            let eps = addrs.into_iter().map(|a| (a, ())).collect();
            if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                trace!("Closed");
                return;
            }
            expiry.await;

            loop {
                match dns.resolve_addrs(na.name().as_ref(), na.port()).await {
                    Ok((addrs, expiry)) => {
                        debug!(?addrs, name = %na);
                        let eps = addrs.into_iter().map(|a| (a, ())).collect();
                        if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                            trace!("Closed");
                            return;
                        }
                        expiry.await;
                    }
                    Err(error) => {
                        debug!(%error);
                        let _ = tx.send(Err(error.into())).await;
                        trace!("Closed");
                        return;
                    }
                }
            }
        }
        .in_current_span(),
    );

    Ok(Box::pin(ReceiverStream::new(rx)))
}
