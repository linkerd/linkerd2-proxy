use super::super::dst::DstAddr;
use super::Endpoint;
use crate::control::destination::{Metadata, Unresolvable};
use crate::proxy::{http::settings, resolve};
use crate::transport::tls;
use crate::{Addr, Conditional, NameAddr};
use futures::{future::Future, try_ready, Async, Poll};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct Resolve<R: resolve::Resolve<NameAddr>>(R);

#[derive(Debug)]
pub struct Resolution<R> {
    resolving: Resolving<R>,
    http_settings: settings::Settings,
}

#[derive(Debug)]
enum Resolving<R> {
    Name {
        dst_logical: Option<NameAddr>,
        dst_concrete: NameAddr,
        resolution: R,
    },
    Unresolvable,
}

// === impl Resolve ===

impl<R> Resolve<R>
where
    R: resolve::Resolve<NameAddr, Endpoint = Metadata>,
{
    pub fn new(resolve: R) -> Self {
        Resolve(resolve)
    }
}

impl<R> resolve::Resolve<DstAddr> for Resolve<R>
where
    R: resolve::Resolve<NameAddr, Endpoint = Metadata>,
    R::Future: Future<Error = Unresolvable>,
{
    type Endpoint = Endpoint;
    type Future = Resolution<R::Future>;
    type Resolution = Resolution<R::Resolution>;

    fn resolve(&self, dst: &DstAddr) -> Self::Future {
        let resolving = match dst.dst_concrete() {
            Addr::Name(ref name) => Resolving::Name {
                dst_logical: dst.dst_logical().name_addr().cloned(),
                dst_concrete: name.clone(),
                resolution: self.0.resolve(&name),
            },
            Addr::Socket(_) => Resolving::Unresolvable,
        };

        Resolution {
            http_settings: dst.http_settings,
            resolving,
        }
    }
}

// === impl Resolution ===

impl<F: Future> Future for Resolution<F>
where
    F: Future<Error = Unresolvable>,
{
    type Item = Resolution<F::Item>;
    type Error = F::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolving = match self.resolving {
            Resolving::Name {
                ref dst_logical,
                ref dst_concrete,
                ref mut resolution,
            } => {
                let res = try_ready!(resolution.poll());
                // TODO: get rid of unnecessary arc bumps?
                Resolving::Name {
                    dst_logical: dst_logical.clone(),
                    dst_concrete: dst_concrete.clone(),
                    resolution: res,
                }
            }
            Resolving::Unresolvable => return Err(Unresolvable::new()),
        };
        Ok(Async::Ready(Resolution {
            resolving,
            // TODO: get rid of unnecessary clone
            http_settings: self.http_settings.clone(),
        }))
    }
}

impl<R> resolve::Resolution for Resolution<R>
where
    R: resolve::Resolution<Endpoint = Metadata>,
{
    type Endpoint = Endpoint;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<resolve::Update<Self::Endpoint>, Self::Error> {
        match self.resolving {
            Resolving::Name {
                ref dst_logical,
                ref dst_concrete,
                ref mut resolution,
            } => match try_ready!(resolution.poll()) {
                resolve::Update::Remove(addr) => {
                    debug!("removing {}", addr);
                    Ok(Async::Ready(resolve::Update::Remove(addr)))
                }
                resolve::Update::Add(addr, metadata) => {
                    let identity = metadata
                        .identity()
                        .cloned()
                        .map(Conditional::Some)
                        .unwrap_or_else(|| {
                            Conditional::None(
                                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                            )
                        });
                    debug!("adding addr={}; identity={:?}", addr, identity);
                    let ep = Endpoint {
                        dst_logical: dst_logical.clone(),
                        dst_concrete: Some(dst_concrete.clone()),
                        addr,
                        identity,
                        metadata,
                        http_settings: self.http_settings,
                    };
                    Ok(Async::Ready(resolve::Update::Add(addr, ep)))
                }
            },
            Resolving::Unresolvable => unreachable!("unresolvable endpoints have no resolutions"),
        }
    }
}
