pub mod metrics;
pub mod respond;

use linkerd_tls as tls;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub(crate) struct ConnectTimeout(pub std::time::Duration);

#[derive(Debug, Error)]
#[error("no identity provided")]
pub struct GatewayIdentityRequired;

#[derive(Debug, Error)]
#[error("bad gateway domain")]
pub struct BadGatewayDomain;

#[derive(Debug, Error)]
#[error("gateway loop detected")]
pub struct GatewayLoop;

#[derive(Debug, Error)]
#[error("required id {required:?}; found {found:?}")]
pub struct OutboundIdentityRequired {
    pub required: tls::client::ServerId,
    pub found: Option<tls::client::ServerId>,
}
