use futures::{Async, Future, Poll};
use futures_watch::{Store, Watch};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_timer::{clock, Delay};
use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};

use api::identity as api;
use never::Never;

pub use identity::{Crt, CrtKey, Csr, InvalidName, Key, Name, TokenSource, TrustAnchors};
use transport::tls;

/// Configures the Identity service and local identity.
#[derive(Clone, Debug)]
pub struct Config {
    pub svc: super::control::ControlAddr,
    pub trust_anchors: TrustAnchors,
    pub key: Key,
    pub csr: Csr,
    pub token: TokenSource,
    pub local_name: Name,
    pub min_refresh: Duration,
    pub max_refresh: Duration,
}

/// Holds the process's local TLS identity state.
///
/// Updates dynamically as certificates are provisioned from the Identity service.
#[derive(Clone, Debug)]
pub struct Local {
    trust_anchors: TrustAnchors,
    name: Name,
    crt_key: Watch<Option<CrtKey>>,
}

/// Produces a `Local` identity once a certificate is available.
#[derive(Debug)]
pub struct AwaitCrt(Option<Local>);

#[derive(Copy, Clone, Debug)]
pub struct LostDaemon;

pub type CrtKeyStore = Store<Option<CrtKey>>;

/// Drives updates.
pub struct Daemon<T>
where
    T: GrpcService<BoxBody>,
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
    T: GrpcService<BoxBody>,
    T::ResponseBody: grpc::Body,
{
    Waiting(Delay),
    ShouldRefresh,
    Pending(grpc::client::unary::ResponseFuture<api::CertifyResponse, T::Future, T::ResponseBody>),
}

// === impl Config ===

impl Config {
    /// Returns a future that fires when a refresh should occur.
    ///
    /// A refresh is scheduled at 70% of the current certificate's lifetime;
    /// though it is never less than min_refresh or larger than max_refresh.
    fn refresh(&self, expiry: SystemTime) -> Delay {
        let now = clock::now();

        let refresh = match expiry
            .duration_since(SystemTime::now())
            .ok()
            .map(|d| d * 7 / 10) // 70% duration
        {
            None => self.min_refresh,
            Some(lifetime) if lifetime < self.min_refresh => self.min_refresh,
            Some(lifetime) if self.max_refresh < lifetime => self.max_refresh,
            Some(lifetime) => lifetime,
        };

        Delay::new(now + refresh)
    }
}

// === impl Local ===

impl Local {
    pub fn new(config: &Config) -> (Self, CrtKeyStore) {
        let (w, s) = Watch::new(None);
        let l = Local {
            name: config.local_name.clone(),
            trust_anchors: config.trust_anchors.clone(),
            crt_key: w,
        };
        (l, s)
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn await_crt(self) -> AwaitCrt {
        AwaitCrt(Some(self))
    }
}

impl tls::client::HasConfig for Local {
    fn tls_client_config(&self) -> Arc<tls::client::Config> {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.tls_client_config();
        }

        self.trust_anchors.tls_client_config()
    }
}

impl tls::listen::HasConfig for Local {
    fn tls_server_name(&self) -> Name {
        self.name.clone()
    }

    fn tls_server_config(&self) -> Arc<tls::listen::Config> {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.tls_server_config();
        }

        tls::listen::empty_config()
    }
}

// === impl Daemon ===

impl<T> Daemon<T>
where
    T: GrpcService<BoxBody> + Clone,
{
    pub fn new(config: Config, crt_key: CrtKeyStore, client: T) -> Self {
        Self {
            config,
            crt_key,
            inner: Inner::ShouldRefresh,
            expiry: UNIX_EPOCH,
            client: api::client::Identity::new(client),
        }
    }
}

impl<T> Future for Daemon<T>
where
    T: GrpcService<BoxBody> + Clone,
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
                    try_ready!(self
                        .client
                        .poll_ready()
                        .map_err(|e| panic!("identity::poll_ready must not fail: {}", e)));

                    match self.config.token.load() {
                        Ok(token) => {
                            let req = grpc::Request::new(api::CertifyRequest {
                                token,
                                identity: self.config.local_name.as_ref().to_owned(),
                                certificate_signing_request: self.config.csr.to_vec(),
                            });
                            let f = self.client.certify(req);
                            Inner::Pending(f)
                        }
                        Err(e) => {
                            error!("Failed to read authentication token: {}", e);
                            Inner::Waiting(self.config.refresh(self.expiry))
                        }
                    }
                }
                Inner::Pending(ref mut p) => match p.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => {
                        let api::CertifyResponse {
                            leaf_certificate,
                            intermediate_certificates,
                            valid_until,
                        } = rsp.into_inner();

                        match valid_until.and_then(|d| Result::<SystemTime, Duration>::from(d).ok())
                        {
                            None => error!(
                                "Identity service did not specify a ceritificate expiration."
                            ),
                            Some(expiry) => {
                                let key = self.config.key.clone();
                                let crt = Crt::new(
                                    self.config.local_name.clone(),
                                    leaf_certificate,
                                    intermediate_certificates,
                                    expiry,
                                );

                                match self.config.trust_anchors.certify(key, crt) {
                                    Err(e) => {
                                        error!("Received invalid ceritficate: {}", e);
                                    }
                                    Ok(crt_key) => {
                                        if self.crt_key.store(Some(crt_key)).is_err() {
                                            // If we can't store a value, than all observations
                                            // have been dropped and we can stop refreshing.
                                            return Ok(Async::Ready(()));
                                        }

                                        self.expiry = expiry;
                                    }
                                }
                            }
                        }

                        Inner::Waiting(self.config.refresh(self.expiry))
                    }
                    Err(e) => {
                        error!("Failed to certify identity: {}", e);
                        Inner::Waiting(self.config.refresh(self.expiry))
                    }
                },
            };
        }
    }
}

// === impl AwaitCrt ===

impl Future for AwaitCrt {
    type Item = Local;
    type Error = LostDaemon;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use futures::Stream;

        let mut local = self.0.take().expect("polled after ready");
        loop {
            if (*local.crt_key.borrow()).is_some() {
                return Ok(Async::Ready(local));
            }

            match local.crt_key.poll() {
                Ok(Async::NotReady) => {
                    self.0 = Some(local);
                    return Ok(Async::NotReady);
                }
                Ok(Async::Ready(Some(()))) => {} // continue
                Err(_) | Ok(Async::Ready(None)) => return Err(LostDaemon),
            }
        }
    }
}
