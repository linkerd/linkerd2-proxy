use crate::TokenSource;
use http_body::Body;
use linkerd2_proxy_api::identity::{self as api, identity_client::IdentityClient};
use linkerd_dns_name::Name;
use linkerd_error::{Error, Result};
use linkerd_identity::{Credentials, DerX509};
use linkerd_proxy_identity_client_metrics::Metrics;
use linkerd_stack::NewService;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::time;
use tonic::{body::BoxBody, client::GrpcService};
use tracing::{debug, error};

/// Configures the Identity service and local identity.
#[derive(Clone, Debug)]
pub struct Config {
    pub token: TokenSource,
    pub min_refresh: Duration,
    pub max_refresh: Duration,
    pub documents: Arc<Documents>,
}

pub struct Documents {
    key_pkcs8: Vec<u8>,
    csr_der: Vec<u8>,
}

#[derive(Copy, Clone, Debug, Error)]
#[error("identity initialization failed")]
pub struct LostDaemon(());

#[derive(Debug)]
pub struct Certify {
    config: Config,
    metrics: Metrics,
}

impl Documents {
    /// Loads a csr.der and key.p8 from the given directory.
    ///
    /// Note that this uses blocking I/O.
    pub fn load(dir: PathBuf) -> std::io::Result<Arc<Self>> {
        let csrp = {
            let mut p = dir.clone();
            p.push("csr");
            p.set_extension("der");
            p
        };
        let csr_der = std::fs::read(csrp).and_then(|b| {
            if b.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "empty CSR",
                ));
            }
            Ok(b)
        })?;

        let keyp = {
            let mut p = dir;
            p.push("key");
            p.set_extension("p8");
            p
        };
        let key_pkcs8 = std::fs::read(keyp).and_then(|b| {
            if b.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "empty key",
                ));
            }
            Ok(b)
        })?;

        Ok(Arc::new(Self { key_pkcs8, csr_der }))
    }
}

// === impl Certify ===

impl From<Config> for Certify {
    fn from(config: Config) -> Self {
        Self {
            config,
            metrics: Metrics::default(),
        }
    }
}

impl Certify {
    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    pub async fn run<C, N, S>(self, name: Name, mut credentials: C, new_client: N)
    where
        C: Credentials,
        N: NewService<(), Service = S>,
        S: GrpcService<BoxBody>,
        S::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
        <S::ResponseBody as Body>::Error: Into<Error> + Send,
    {
        debug!("Identity daemon running");
        let mut curr_expiry = UNIX_EPOCH;

        loop {
            debug!("Certifying identity");
            let crt = {
                // The client is used for infrequent communication with the identity controller;
                // so clients are instantiated on-demand rather than held.
                let client = new_client.new_service(());
                certify(
                    &self.config.token,
                    &self.config.documents,
                    client,
                    &name,
                    &mut credentials,
                )
                .await
            };

            match crt {
                Ok(expiry) => {
                    debug!(?expiry, "Identity certified");
                    self.metrics.refresh(expiry);
                    curr_expiry = expiry
                }
                Err(error) => {
                    error!(error, "Failed to obtain identity");
                }
            }

            let sleep = refresh_in(&self.config, curr_expiry);
            debug!(?sleep, "Waiting to refresh identity");
            time::sleep(sleep).await;
        }
    }
}

// === impl Documents ===

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Documents")
            .field("key_pkcs8", &"...")
            .field("csr_der", &"...")
            .finish()
    }
}

/// Issues a certificate signing request to the identity service with a token loaded from the token
/// source.
async fn certify<C, S>(
    token: &TokenSource,
    docs: &Documents,
    client: S,
    name: &Name,
    credentials: &mut C,
) -> Result<SystemTime>
where
    C: Credentials,
    S: GrpcService<BoxBody>,
    S::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as Body>::Error: Into<Error> + Send,
{
    let req = tonic::Request::new(api::CertifyRequest {
        token: token.load()?,
        identity: name.to_string(),
        certificate_signing_request: docs.csr_der.clone(),
    });

    let api::CertifyResponse {
        leaf_certificate,
        intermediate_certificates,
        valid_until,
    } = IdentityClient::new(client).certify(req).await?.into_inner();

    let exp = valid_until.ok_or("identity certification missing expiration")?;
    let expiry = SystemTime::try_from(exp)?;
    if expiry <= SystemTime::now() {
        return Err("certificate already expired".into());
    }
    credentials.set_certificate(
        DerX509(leaf_certificate),
        intermediate_certificates.into_iter().map(DerX509).collect(),
        docs.key_pkcs8.clone(),
    )?;

    Ok(expiry)
}

/// Returns a future that fires when a refresh should occur.
///
/// A refresh is scheduled at 70% of the current certificate's lifetime;
/// though it is never less than min_refresh or larger than max_refresh.
fn refresh_in(config: &Config, expiry: SystemTime) -> Duration {
    match expiry
        .duration_since(SystemTime::now())
        .ok()
        .map(|d| d * 7 / 10) // 70% duration
    {
        None => config.min_refresh,
        Some(lifetime) if lifetime < config.min_refresh => config.min_refresh,
        Some(lifetime) if config.max_refresh < lifetime => config.max_refresh,
        Some(lifetime) => lifetime,
    }
}
