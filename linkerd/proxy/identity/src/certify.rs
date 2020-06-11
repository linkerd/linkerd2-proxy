use crate::{Crt, CrtKey, Csr, Key, Name, TokenSource, TrustAnchors};
use http_body::Body as HttpBody;
use linkerd2_error::Error;
use linkerd2_proxy_api::identity as api;
use linkerd2_proxy_transport::tls;
use pin_project::pin_project;
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::{self, Delay};
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
}

/// Produces a `Local` identity once a certificate is available.
#[derive(Debug)]
pub struct AwaitCrt(Option<Local>);

#[derive(Copy, Clone, Debug)]
pub struct LostDaemon;

pub type CrtKeySender = watch::Sender<Option<CrtKey>>;

pub async fn daemon<T>(config: Config, crt_key_watch: watch::Sender<Option<CrtKey>>, client: T)
where
    T: GrpcService<BoxBody>,
    T::ResponseBody: Send + 'static,
    <T::ResponseBody as Body>::Data: Send,
    <T::ResponseBody as HttpBody>::Error: Into<Error> + Send,
{
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
                            None => {
                                error!("Identity service did not specify a certificate expiration.")
                            }
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
                                        if crt_key_watch.broadcast(Some(crt_key)).is_err() {
                                            // If we can't store a value, than all observations
                                            // have been dropped and we can stop refreshing.
                                            return;
                                        }

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

// // === impl Config ===

impl Config {
    /// Returns a future that fires when a refresh should occur.
    ///
    /// A refresh is scheduled at 70% of the current certificate's lifetime;
    /// though it is never less than min_refresh or larger than max_refresh.
    fn refresh(&self, expiry: SystemTime) -> Delay {
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
        time::delay_for(refresh)
    }
}

// === impl Local ===

impl Local {
    pub fn new(config: &Config) -> (Self, CrtKeySender) {
        let (s, w) = watch::channel(None);
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

    pub async fn await_crt(mut self) -> Result<Self, LostDaemon> {
        while self.crt_key.borrow().is_none() {
            // If the sender is dropped, the daemon task has ended.
            if let None = self.crt_key.recv().await {
                return Err(LostDaemon);
            }
        }
        return Ok(self);
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

impl tls::accept::HasConfig for Local {
    fn tls_server_name(&self) -> Name {
        self.name.clone()
    }

    fn tls_server_config(&self) -> Arc<tls::accept::Config> {
        if let Some(ref c) = *self.crt_key.borrow() {
            return c.tls_server_config();
        }

        tls::accept::empty_config()
    }
}
