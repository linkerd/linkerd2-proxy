// These crates are only used within the `tls` module.
extern crate rustls;
extern crate tokio_rustls;
extern crate untrusted;
extern crate webpki;

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
