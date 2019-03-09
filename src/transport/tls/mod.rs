// These crates are only used within the `tls` module.
extern crate rustls;
extern crate tokio_rustls;
extern crate untrusted;
extern crate webpki;

use std::fmt;

use Conditional;

mod cert_resolver;
pub mod conditional_accept;
mod config;
mod connection;
mod identity;

pub use self::{
    config::{
        ClientConfig, ClientConfigWatch, CommonSettings, ConditionalClientConfig,
        ConditionalConnectionConfig, ConfigWatch, ConnectionConfig, Error as ConfigError,
        ReasonForNoIdentity, ReasonForNoTls, ServerConfig, ServerConfigWatch,
    },
    connection::{Connection, Session, UpgradeClientToTls, UpgradeServerToTls},
    identity::Identity,
    rustls::TLSError as Error,
};

#[cfg(test)]
pub use self::config::test_util as config_test_util;

pub type ConditionalIdentity = Conditional<Identity, ReasonForNoTls>;

/// Describes whether or not a connection was secured with TLS and, if it was
/// not, the reason why.
pub type Status = Conditional<(), ReasonForNoTls>;

impl Status {
    pub fn from<C>(c: &Conditional<C, ReasonForNoTls>) -> Self
    where
        C: Clone + fmt::Debug,
    {
        c.as_ref().map(|_| ())
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match *self {
            Conditional::Some(()) => "true",
            Conditional::None(ReasonForNoTls::NoConfig) => "no_config",
            Conditional::None(ReasonForNoTls::Disabled) => "disabled",
            Conditional::None(ReasonForNoTls::InternalTraffic) => "internal_traffic",
            Conditional::None(ReasonForNoTls::NoIdentity(_)) => "no_identity",
            Conditional::None(ReasonForNoTls::NotProxyTls) => "no_proxy_tls",
        };

        f.pad(s)
    }
}

/// Fetch the `tls::Status` of this type.
pub trait HasStatus {
    fn tls_status(&self) -> Status;
}

fn parse_end_entity_cert<'a>(
    cert_chain: &'a [rustls::Certificate],
) -> Result<webpki::EndEntityCert<'a>, webpki::Error> {
    let cert = cert_chain
        .first()
        .map(rustls::Certificate::as_ref)
        .unwrap_or(&[]); // An empty input will fail to parse.
    webpki::EndEntityCert::from(untrusted::Input::from(cert))
}
