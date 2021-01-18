use http_body::Body as HttpBody;
use linkerd2_proxy_api::identity as api;
use linkerd_error::Error;
use linkerd_identity as id;
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
    pub trust_anchors: id::TrustAnchors,
    pub key: id::Key,
    pub csr: id::Csr,
    pub token: id::TokenSource,
    pub local_id: id::LocalId,
    pub min_refresh: Duration,
    pub max_refresh: Duration,
}

/// Holds the process's local TLS identity state.
///
/// Updates dynamically as certificates are provisioned from the Identity service.
#[pin_project]
#[derive(Clone, Debug)]
pub struct Local {
    trust_anchors: id::TrustAnchors,
    id: id::LocalId,
    crt_key: watch::Receiver<Option<id::CrtKey>>,
    refreshes: Arc<Counter>,
}

/// Produces a `Local` identity once a certificate is available.
#[derive(Debug)]
pub struct AwaitCrt(Option<Local>);

#[derive(Copy, Clone, Debug)]
pub struct LostDaemon;

pub type CrtKeySender = watch::Sender<Option<id::CrtKey>>;

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
            crt_key_watch,
            refreshes,
            config,
        } = self;

        let mut curr_expiry = UNIX_EPOCH;
        let mut client = api::identity_client::IdentityClient::new(client);

        loop {
            match config.token.load() {
                Ok(token) => {
                    let req = grpc::Request::new(api::CertifyRequest {
                        token,
                        identity: config.local_id.to_string(),
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
                                    let crt = id::Crt::new(
                                        config.local_id.clone(),
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
            id: config.local_id.clone(),
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

    pub fn name(&self) -> &id::Name {
        self.id.as_ref()
    }
}

impl Into<tls::client::Config> for &'_ Local {
    fn into(self) -> tls::client::Config {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.into();
        }

        (&self.trust_anchors).into()
    }
}

impl Into<id::LocalId> for &'_ Local {
    fn into(self) -> id::LocalId {
        id::LocalId(self.name().clone())
    }
}

impl Into<tls::server::Config> for &'_ Local {
    fn into(self) -> tls::server::Config {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.into();
        }

        tls::server::empty_config()
    }
}
