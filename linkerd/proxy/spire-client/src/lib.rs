#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509};
use spiffe::{svid::x509::X509Svid, workload_api::client::WorkloadApiClient};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time;
use tracing::{debug, error, warn};

#[derive(Debug)]
pub struct Spire {
    socket: Arc<String>,
}

// === impl Spire ===

impl Spire {
    pub fn new(socket: Arc<String>) -> Self {
        Self { socket }
    }

    pub async fn run<C>(self, mut credentials: C)
    where
        C: Credentials,
    {
        loop {
            debug!("Obtaining SPIFFE identity");
            match listen_for_updates(&self.socket, &mut credentials).await {
                Ok(()) => warn!("stream closed"),
                Err(error) => error!("stream failed: {}", error),
            }

            time::sleep(time::Duration::from_secs(10)).await
        }
    }
}

async fn listen_for_updates<C>(socket: &str, credentials: &mut C) -> Result<()>
where
    C: Credentials,
{
    let mut client = WorkloadApiClient::new_from_path(socket).await?;
    let mut stream = client.stream_x509_contexts().await?;

    while let Some(x509_context_update) = stream.next().await {
        match x509_context_update {
            Ok(update) => {
                if let Some(svid) = update.default_svid() {
                    match process_svid(credentials, svid) {
                        Ok(_expiry) => {
                            // refresh metrics when we have them here
                        }
                        Err(error) => {
                            warn!("error processing SVID update: {}", error)
                        }
                    }
                } else {
                    warn!("received update with no SVIDs")
                }
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

fn process_svid<C>(credentials: &mut C, svid: &X509Svid) -> Result<SystemTime>
where
    C: Credentials,
{
    use x509_parser::prelude::*;

    let chain: Vec<DerX509> = svid
        .cert_chain()
        .iter()
        .skip(1)
        .map(|c| DerX509(c.content().to_vec()))
        .collect();

    let (_, parsed_cert) = X509Certificate::from_der(svid.leaf().content())?;
    let exp: u64 = parsed_cert.validity().not_after.timestamp().try_into()?;
    let exp = UNIX_EPOCH + Duration::from_secs(exp);

    let leaf = DerX509(svid.leaf().content().to_vec());
    let key = svid.private_key().content().to_vec();

    credentials.set_certificate(leaf, chain, key)?;
    Ok(exp)
}
