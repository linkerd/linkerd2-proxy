use crate::{Crt, CrtKey, Csr, Key, Name, TokenSource, TrustAnchors};
use http_body::Body as HttpBody;
use linkerd2_proxy_api::identity as api;
use linkerd_error::Error;
use linkerd_metrics::Counter;
use linkerd_tls as tls;
use pin_project::pin_project;
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::{self, Sleep};
use tonic::{
    self as grpc,
    body::{Body, BoxBody},
    client::GrpcService,
};
use tracing::{debug, error, trace};

/// Configures the Identity service and local identity.
#[derive(Clone, Debug)]
pub struct Config {
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
#[pin_project]
#[derive(Clone, Debug)]
pub struct Local {
    trust_anchors: TrustAnchors,
    name: Name,
    crt_key: watch::Receiver<Option<CrtKey>>,
    refreshes: Arc<Counter>,
}

/// Produces a `Local` identity once a certificate is available.
#[derive(Debug)]
pub struct AwaitCrt(Option<Local>);

#[derive(Copy, Clone, Debug)]
pub struct LostDaemon;

pub type CrtKeySender = watch::Sender<Option<CrtKey>>;

#[derive(Debug)]
pub struct Daemon {
    crt_key_watch: CrtKeySender,
    refreshes: Arc<linkerd_metrics::Counter>,
    config: Config,
}

// === impl Config ===

impl Config {
    /// Returns a future that fires when a refresh should occur.
    ///
    /// A refresh is scheduled at 70% of the current certificate's lifetime;
    /// though it is never less than min_refresh or larger than max_refresh.
    fn refresh(&self, expiry: SystemTime) -> Sleep {
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
        trace!("will refresh in {:?}", refresh);
        time::sleep(refresh)
    }
}

// === impl Daemon ===

impl Daemon {
    pub async fn run<T>(self, client: T)
    where
        T: GrpcService<BoxBody>,
        T::ResponseBody: Send + 'static,
        <T::ResponseBody as Body>::Data: Send,
        <T::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    {
        let Self {
            config,
            crt_key_watch,
            refreshes,
        } = self;
        let mut curr_expiry = UNIX_EPOCH;
        let mut client = api::identity_client::IdentityClient::new(client);

        loop {
            match config.token.load() {
                Ok(token) => {
                    let req = grpc::Request::new(api::CertifyRequest {
                        token,
                        identity: config.local_name.as_ref().to_owned(),
                        certificate_signing_request: config.csr.to_vec(),
                    });
                    trace!("daemon certifying");
                    let rsp = client.certify(req).await;
                    match rsp {
                        Err(e) => error!("Failed to certify identity: {}", e),
                        Ok(rsp) => {
                            let api::CertifyResponse {
                                leaf_certificate,
                                intermediate_certificates,
                                valid_until,
                            } = rsp.into_inner();
                            match valid_until.and_then(|d| SystemTime::try_from(d).ok()) {
                                None => error!(
                                    "Identity service did not specify a certificate expiration."
                                ),
                                Some(expiry) => {
                                    let key = config.key.clone();
                                    let crt = Crt::new(
                                        config.local_name.clone(),
                                        leaf_certificate,
                                        intermediate_certificates,
                                        expiry,
                                    );

                                    match config.trust_anchors.certify(key, crt) {
                                        Err(e) => {
                                            error!("Received invalid ceritficate: {}", e);
                                        }
                                        Ok(crt_key) => {
                                            debug!("daemon certified until {:?}", expiry);
                                            if crt_key_watch.send(Some(crt_key)).is_err() {
                                                // If we can't store a value, than all observations
                                                // have been dropped and we can stop refreshing.
                                                return;
                                            }

                                            refreshes.incr();
                                            curr_expiry = expiry;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => error!("Failed to read authentication token: {}", e),
            }
            config.refresh(curr_expiry).await;
        }
    }
}

// === impl Local ===

impl Local {
    pub fn new(config: &Config) -> (Self, Daemon) {
        let (s, w) = watch::channel(None);
        let refreshes = Arc::new(Counter::new());
        let l = Local {
            name: config.local_name.clone(),
            trust_anchors: config.trust_anchors.clone(),
            crt_key: w,
            refreshes: refreshes.clone(),
        };
        let daemon = Daemon {
            config: config.clone(),
            refreshes,
            crt_key_watch: s,
        };
        (l, daemon)
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub async fn await_crt(mut self) -> Result<Self, LostDaemon> {
        while self.crt_key.borrow().is_none() {
            // If the sender is dropped, the daemon task has ended.
            if self.crt_key.changed().await.is_err() {
                return Err(LostDaemon);
            }
        }
        Ok(self)
    }

    pub fn metrics(&self) -> crate::metrics::Report {
        crate::metrics::Report::new(self.crt_key.clone(), self.refreshes.clone())
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

impl tls::server::HasConfig for Local {
    fn tls_server_name(&self) -> Name {
        self.name.clone()
    }

    fn tls_server_config(&self) -> Arc<tls::server::Config> {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.tls_server_config();
        }

        tls::server::empty_config()
    }
}
