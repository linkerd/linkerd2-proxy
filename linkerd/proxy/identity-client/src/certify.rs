use crate::{Metrics, TokenSource};
use http_body::Body;
use linkerd2_proxy_api::identity::{self as api, identity_client::IdentityClient};
use linkerd_error::{Error, Result};
use linkerd_identity::{Credentials, DerX509};
use linkerd_stack::NewService;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
}

#[derive(Copy, Clone, Debug, Error)]
#[error("identity initialization failed")]
pub struct LostDaemon(());

#[derive(Debug)]
pub struct Certify {
    config: Config,
    metrics: Metrics,
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

    pub async fn run<C, N, S>(self, mut credentials: C, new_client: N)
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
            let crt = certify(
                &self.config.token,
                // The client is used for infrequent communication with the identity controller;
                // so clients are instantiated on-demand rather than held.
                new_client.new_service(()),
                &mut credentials,
            )
            .await;

            match crt {
                Ok(expiry) => {
                    debug!(?expiry, "Identity certified");
                    self.metrics.refresh(expiry);
                    curr_expiry = expiry
                }
                Err(error) => {
                    error!(%error, "Failed to obtain identity");
                }
            }

            let sleep = refresh_in(&self.config, curr_expiry);
            debug!(?sleep, "Waiting to refresh identity");
            time::sleep(sleep).await;
        }
    }
}

/// Issues a certificate signing request to the identity service with a token loaded from the token
/// source.
async fn certify<C, S>(token: &TokenSource, client: S, credentials: &mut C) -> Result<SystemTime>
where
    C: Credentials,
    S: GrpcService<BoxBody>,
    S::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as Body>::Error: Into<Error> + Send,
{
    let req = tonic::Request::new(api::CertifyRequest {
        token: token.load()?,
        identity: credentials.dns_name().to_string(),
        certificate_signing_request: credentials.gen_certificate_signing_request().to_vec(),
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
        expiry,
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
