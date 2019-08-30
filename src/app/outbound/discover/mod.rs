use crate::app::{dst::DstAddr, outbound::Endpoint};
use crate::core::resolve;
use crate::proxy::http::settings;
use crate::resolve_dst_api::Metadata;
use crate::svc::Service;
use crate::transport::tls;
use crate::{Addr, Conditional, Error, NameAddr};
use futures::{try_ready, Async, Future, Poll, Stream};

#[derive(Clone, Debug)]
pub struct Resolve<R: resolve::Resolve<NameAddr>>(R);

#[derive(Debug)]
pub struct ResolveFuture<F>(Inner<F>);

#[derive(Debug)]
enum Inner<F> {
    Unresolvable,
    Future { inner: F, meta: Option<Meta> },
}

#[derive(Debug)]
pub struct Resolution<R> {
    inner: R,
    meta: Meta,
}

#[derive(Copy, Clone, Debug)]
pub struct Unresolvable(());

#[derive(Clone, Debug)]
struct Meta {
    dst_logical: Option<NameAddr>,
    dst_concrete: NameAddr,
    http_settings: settings::Settings,
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

impl<R> Service<DstAddr> for Resolve<R>
where
    R: resolve::Resolve<NameAddr, Endpoint = Metadata>,
{
    type Response = Resolution<R::Resolution>;
    type Error = Error;
    type Future = ResolveFuture<R::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, dst: DstAddr) -> Self::Future {
        match dst.dst_concrete() {
            Addr::Socket(_) => ResolveFuture(Inner::Unresolvable),
            Addr::Name(ref name) => {
                let inner = self.0.resolve(name.clone());
                let meta = Meta {
                    dst_logical: dst.dst_logical().name_addr().cloned(),
                    dst_concrete: name.clone(),
                    http_settings: dst.http_settings.clone(),
                };
                ResolveFuture(Inner::Future {
                    inner,
                    meta: Some(meta),
                })
            }
        }
    }
}

// === impl ResolveFuture ===

impl<F: Future> Future for ResolveFuture<F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = Resolution<F::Item>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0 {
            Inner::Unresolvable => return Err(Unresolvable(()).into()),
            Inner::Future {
                ref mut inner,
                ref mut meta,
            } => {
                let inner = try_ready!(inner.poll().map_err(Into::into));
                let resolution = Resolution {
                    inner,
                    meta: meta.take().expect("poll after ready"),
                };
                Ok(resolution.into())
            }
        }
    }
}

// === impl Resolution ===

impl<R> Stream for Resolution<R>
where
    R: resolve::Resolution<Endpoint = Metadata>,
{
    type Item = resolve::Update<Endpoint>;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match try_ready!(self.inner.poll()) {
            resolve::Update::DoesNotExist => Ok(Some(resolve::Update::DoesNotExist).into()),
            resolve::Update::Empty => Ok(Some(resolve::Update::Empty).into()),
            resolve::Update::Remove(rms) => Ok(Some(resolve::Update::Remove(rms)).into()),
            resolve::Update::Add(adds) => {
                let endpoints = adds.into_iter().map(|(addr, metadata)| {
                    let identity = metadata
                        .identity()
                        .cloned()
                        .map(Conditional::Some)
                        .unwrap_or_else(|| {
                            Conditional::None(
                                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                            )
                        });
                    let ep = Endpoint {
                        addr,
                        identity,
                        metadata,
                        dst_logical: self.meta.dst_logical.clone(),
                        dst_concrete: Some(self.meta.dst_concrete.clone()),
                        http_settings: self.meta.http_settings.clone(),
                    };
                    (addr, ep)
                });
                Ok(Async::Ready(Some(resolve::Update::Add(
                    endpoints.collect(),
                ))))
            }
        }
    }
}

/// === impl Unresolvable ===

impl std::fmt::Display for Unresolvable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unresolvable destination")
    }
}

impl std::error::Error for Unresolvable {}
