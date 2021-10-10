use http_body::Body;
use linkerd2_proxy_api::identity::{self as api, identity_client::IdentityClient};
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_metrics::Counter;
use linkerd_stack::{NewService, Param};
use linkerd_tls as tls;
use pin_project::pin_project;
use std::{
    convert::TryFrom,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::{self, Sleep};
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
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
pub struct LocalCrtKey {
    trust_anchors: id::TrustAnchors,
    id: id::LocalId,
    crt_key: watch::Receiver<Option<id::CrtKey>>,
    refreshes: Arc<Counter>,
}

/// Produces a `Local` identity once a certificate is available.
#[derive(Debug)]
pub struct AwaitCrt(Option<LocalCrtKey>);

#[derive(Copy, Clone, Debug, Error)]
#[error("identity initialization failed")]
pub struct LostDaemon(());

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
    pub async fn run<N, S>(self, new_client: N)
    where
        N: NewService<(), Service = S>,
        S: GrpcService<BoxBody>,
        S::ResponseBody: Send + Sync + 'static,
        <S::ResponseBody as Body>::Data: Send,
        <S::ResponseBody as Body>::Error: Into<Error> + Send,
    {
        let Self {
            crt_key_watch,
            refreshes,
            config,
        } = self;

        debug!("Identity daemon running");
        let mut curr_expiry = UNIX_EPOCH;

        loop {
            match config.token.load() {
                Ok(token) => {
                    let rsp = {
                        // The client is used for infrequent communication with the identity controller;
                        // so clients are instantiated on-demand rather than held.
                        let mut client = IdentityClient::new(new_client.new_service(()));

                        trace!("daemon certifying");
                        let req = grpc::Request::new(api::CertifyRequest {
                            token,
                            identity: config.local_id.to_string(),
                            certificate_signing_request: config.csr.to_vec(),
                        });
                        client.certify(req).await
                    };

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
                                            error!("Received invalid certificate: {}", e);
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

// === impl LocalCrtKey ===

impl LocalCrtKey {
    pub fn new(config: &Config) -> (Self, Daemon) {
        let (s, w) = watch::channel(None);
        let refreshes = Arc::new(Counter::new());
        let l = Self {
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

    #[cfg(feature = "test-util")]
    pub fn for_test(id: &id::test_util::Identity) -> Self {
        let crt_key = id.validate().expect("Identity must be valid");
        let (tx, rx) = watch::channel(Some(crt_key));
        // Prevent the receiver stream from ending.
        tokio::spawn(async move {
            tx.closed().await;
        });
        Self {
            id: id.id(),
            trust_anchors: id.trust_anchors(),
            crt_key: rx,
            refreshes: Arc::new(Counter::new()),
        }
    }

    #[cfg(feature = "test-util")]
    pub fn default_for_test() -> Self {
        Self::for_test(&id::test_util::DEFAULT_DEFAULT)
    }

    pub async fn await_crt(mut self) -> Result<Self, LostDaemon> {
        while self.crt_key.borrow().is_none() {
            // If the sender is dropped, the daemon task has ended.
            if self.crt_key.changed().await.is_err() {
                return Err(LostDaemon(()));
            }
        }
        Ok(self)
    }

    pub fn metrics(&self) -> crate::metrics::Report {
        crate::metrics::Report::new(self.crt_key.clone(), self.refreshes.clone())
    }

    pub fn id(&self) -> &id::LocalId {
        &self.id
    }

    pub fn name(&self) -> &id::Name {
        &self.id.0
    }

    pub fn client_config(&self) -> Arc<tls::rustls::ClientConfig> {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.client_config();
        }

        self.trust_anchors.client_config()
    }

    pub fn server_config(&self) -> Arc<tls::rustls::ServerConfig> {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.server_config();
        }

        let verifier = tls::rustls::NoClientAuth::new();
        Arc::new(tls::rustls::ServerConfig::new(verifier))
    }
}

impl NewService<tls::ClientTls> for LocalCrtKey {
    type Service = tls::rustls::Connect;

    fn new_service(&self, target: tls::ClientTls) -> Self::Service {
        // TODO: ALPN
        tls::rustls::Connect::new(target, self.client_config())
    }
}

impl Param<Arc<tls::rustls::ServerConfig>> for LocalCrtKey {
    fn param(&self) -> Arc<tls::rustls::ServerConfig> {
        self.server_config()
    }
}

impl Param<id::LocalId> for LocalCrtKey {
    fn param(&self) -> id::LocalId {
        self.id().clone()
    }
}
