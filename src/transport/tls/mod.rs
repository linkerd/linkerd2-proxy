// These crates are only used within the `tls` module.
extern crate rustls;
extern crate tokio_rustls;
extern crate untrusted;
extern crate webpki;

use std::fmt;

use Conditional;

pub mod conditional_accept;
mod config;
mod cert_resolver;
mod connection;
mod dns_name;
mod identity;

pub use self::{
    config::{
        ClientConfig,
        ClientConfigWatch,
        CommonSettings,
        ConditionalConnectionConfig,
        ConditionalClientConfig,
        ConfigWatch,
        ConnectionConfig,
        Error as ConfigError,
        ReasonForNoTls,
        ReasonForNoIdentity,
        ServerConfig,
        ServerConfigWatch,
    },
    connection::{
        Connection,
        Session,
        UpgradeClientToTls,
        UpgradeServerToTls
    },
    dns_name::{DnsName, InvalidDnsName},
    identity::Identity,
    rustls::TLSError as Error,
};

#[cfg(test)]
pub use self::config::test_util as config_test_util;

/// Describes whether or not a connection was secured with TLS and, if it was
/// not, the reason why.
pub type Status = Conditional<(), ReasonForNoTls>;

impl Status {
    pub fn from<C>(c: &Conditional<C, ReasonForNoTls>) -> Self
    where
        C: Clone + fmt::Debug
    {
        c.as_ref().map(|_| ())
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match *self {
            Conditional::Some(()) => "true",
            Conditional::None(ReasonForNoTls::NoConfig) => "no_config",
            Conditional::None(ReasonForNoTls::HandshakeFailed) => "handshake_failed",
            Conditional::None(ReasonForNoTls::Disabled) => "disabled",
            Conditional::None(ReasonForNoTls::InternalTraffic) => "internal_traffic",
            Conditional::None(ReasonForNoTls::NoIdentity(_)) => "no_identity",
            Conditional::None(ReasonForNoTls::NotProxyTls) => "no_proxy_tls"
        };

        f.pad(s)
    }
}

