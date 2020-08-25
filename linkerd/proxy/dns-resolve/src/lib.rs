#![deny(warnings, rust_2018_idioms)]

use futures::{future, prelude::*, stream};
use linkerd2_addr::Addr;
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
        let addr = match target.into() {
            Addr::Name(na) if na.is_localhost() => {
                SocketAddr::from(([127, 0, 0, 1], na.port())).into()
            }
            addr => addr,
        };

        match addr {
            Addr::Socket(sa) => {
                let eps = vec![(sa, ())];
                let updates: UpdateStream = Box::pin(stream::iter(Some(Ok(Update::Reset(eps)))));
                Box::pin(future::ok(updates))
            }
            Addr::Name(na) => {
                let dns = self.dns.clone();
                Box::pin(async move {
                    let (mut tx, rx) = mpsc::channel::<Result<Update<()>, Error>>(1);

                    // First, try to resolve the name via SRV records.
                    match dns.resolve_srv(na.name().clone()).await {
                        Ok((addrs, expiry)) => {
                            tokio::spawn(async move {
                                let eps = addrs.into_iter().map(|a| (a, ())).collect();
                                if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                                    return;
                                }
                                expiry.await;
                                loop {
                                    match dns.resolve_srv(na.name().clone()).await {
                                        Ok((addrs, expiry)) => {
                                            let eps = addrs.into_iter().map(|a| (a, ())).collect();
                                            if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                                                return;
                                            }
                                            expiry.await;
                                        }
                                        Err(e) => {
                                            let _ = tx.send(Err(e)).await;
                                            return;
                                        }
                                    }
                                }
                            });
                        }

                        // If the initial SRV resolution failed because we
                        // couldn't parse out IPs from the response, try falling
                        // back to normal-old A records.
                        Err(e) if e.is::<dns::InvalidSrv>() => {
                            let port = na.port();
                            let (ips, expiry) = dns.resolve_a(na.name().clone()).await?;

                            tokio::spawn(async move {
                                let eps = ips
                                    .into_iter()
                                    .map(|i| (SocketAddr::new(i, port), ()))
                                    .collect();
                                if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                                    return;
                                }
                                expiry.await;
                                loop {
                                    match dns.resolve_a(na.name().clone()).await {
                                        Ok((ips, expiry)) => {
                                            let eps = ips
                                                .into_iter()
                                                .map(|i| (SocketAddr::new(i, port), ()))
                                                .collect();
                                            if tx.send(Ok(Update::Reset(eps))).await.is_err() {
                                                return;
                                            }
                                            expiry.await;
                                        }
                                        Err(e) => {
                                            let _ = tx.send(Err(e.into())).await;
                                            return;
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => return Err(e),
                    }

                    let updates: UpdateStream = Box::pin(rx);
                    Ok(updates)
                })
            }
        }
    }
}
