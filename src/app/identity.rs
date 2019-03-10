use bytes::IntoBuf;
use futures::sync::mpsc;
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use futures_watch::{Store, Watch};
use http;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::executor::{DefaultExecutor, Executor};
use tokio_timer::{clock, Delay};
use tower_grpc::{self as grpc, Body, BoxBody};
use tower_http::HttpService;

use api::identity as api;
use never::Never;

use identity;
pub use identity::{CrtKey, Identity, Key, TokenSource, TrustAnchors, CSR};

#[derive(Debug)]
pub struct Config {
    pub trust_anchors: TrustAnchors,
    pub key: Key,
    pub csr: CSR,
    pub identity: Identity,
    pub token: TokenSource,
    pub min_refresh: Duration,
    pub max_refresh: Duration,
}

/// Drives updates.
pub struct Daemon<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: grpc::Body,
{
    config: Config,
    client: api::client::Identity<T>,
    crt_key: Store<Option<CrtKey>>,
    expiry: SystemTime,
    inner: Inner<T>,
}

enum Inner<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: grpc::Body,
{
    Waiting(Delay),
    ShouldRefresh,
    Pending(grpc::client::unary::ResponseFuture<api::CertifyResponse, T::Future, T::ResponseBody>),
}

pub fn new<T>(config: Config, client: T) -> (Watch<Option<CrtKey>>, Daemon<T>)
where
    T: HttpService<BoxBody>,
    T::ResponseBody: grpc::Body,
{
    let (w, crt_key) = Watch::new(None);
    let d = Daemon {
        config,
        crt_key,
        inner: Inner::ShouldRefresh,
        expiry: SystemTime::now(),
        client: api::client::Identity::new(client),
    };
    (w, d)
}

// === impl Daemon ===

impl<T> Daemon<T>
where
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    fn refresh(expiry: SystemTime, min_refresh: Duration) -> Delay {
        // Take the current instant from the clock before
        // snapshotting the SystemTime so that the, if anything,
        // we understimate the time to sleep.
        let now = clock::now();

        // Use 80% of the actual lifetime, so that we can tolerate failures and retry...
        let lifetime = expiry
            .duration_since(SystemTime::now())
            .map(|d| d * 8 / 10)
            .unwrap_or_else(|_| min_refresh);

        Delay::new(now + lifetime)
    }
}

impl<T> Future for Daemon<T>
where
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.inner = match self.inner {
                Inner::Waiting(ref mut d) => {
                    if let Ok(Async::NotReady) = d.poll() {
                        return Ok(Async::NotReady);
                    }
                    Inner::ShouldRefresh
                }
                Inner::ShouldRefresh => {
                    let req = grpc::Request::new(api::CertifyRequest {
                        identity: self.config.identity.as_ref().to_owned(),
                        token: self.config.token.load().expect("FIXME"),
                        certificate_signing_request: self.config.csr.to_vec(),
                    });
                    let f = self.client.certify(req);
                    Inner::Pending(f)
                }
                Inner::Pending(ref mut p) => match p.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => {
                        let api::CertifyResponse {
                            leaf_certificate,
                            intermediate_certificates,
                            valid_until,
                        } = rsp.into_inner();

                        let crt_key = self
                            .config
                            .key
                            .to_crt_key(leaf_certificate, intermediate_certificates);
                        // TODO validate crt_key against roots.

                        if self.crt_key.store(Some(crt_key)).is_err() {
                            // If we can't store a value, than all observations
                            // have been dropped and we can stop refreshing.
                            return Ok(Async::Ready(()));
                        }

                        self.expiry = valid_until
                            .and_then(|ts| Result::<SystemTime, Duration>::from(ts).ok())
                            .unwrap_or(UNIX_EPOCH);

                        Inner::Waiting(Self::refresh(self.expiry, self.config.min_refresh))
                    }
                    Err(e) => {
                        error!("Failed to certify identity: {}", e);
                        Inner::Waiting(Self::refresh(self.expiry, self.config.min_refresh))
                    }
                },
            };
        }
    }
}
