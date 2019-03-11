use std::{self, fmt, path::PathBuf, sync::Arc};

use super::{rustls, webpki};
use identity;

use futures_watch::Watch;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasonForNoTls {
    /// TLS is disabled.
    Disabled,

    /// TLS was enabled but the configuration isn't available (yet).
    NoConfig,

    /// The endpoint's TLS identity is unknown. Without knowing its identity
    /// we can't validate its certificate.
    NoIdentity(ReasonForNoIdentity),

    /// The connection is between the proxy and the service
    InternalTraffic,

    /// The connection isn't TLS or it is TLS but not intended to be handled
    /// by the proxy.
    NotProxyTls,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasonForNoIdentity {
    /// The connection is a non-HTTP connection so we don't know anything
    /// about the destination besides its address.
    NotHttp,

    /// The connection is for HTTP but the HTTP request doesn't have an
    /// authority so we can't extract the identity from it.
    NoAuthorityInHttpRequest,

    /// The destination service didn't give us the identity, which is its way
    /// of telling us that we shouldn't do TLS for this endpoint.
    NotProvidedByServiceDiscovery,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    /// The proxy wasn't configured with the identity.
    NotConfigured,

    /// We haven't implemented the mechanism to construct a TLs identity for
    /// the tap psuedo-service yet.
    NotImplementedForTap,

    /// We haven't implemented the mechanism to construct a TLs identity for
    /// the metrics psuedo-service yet.
    NotImplementedForMetrics,
}

impl From<ReasonForNoIdentity> for ReasonForNoTls {
    fn from(r: ReasonForNoIdentity) -> Self {
        ReasonForNoTls::NoIdentity(r)
    }
}

impl fmt::Display for ReasonForNoIdentity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ReasonForNoIdentity::NotHttp => f.pad("not_http"),
            ReasonForNoIdentity::NoAuthorityInHttpRequest => f.pad("no_authority_in_http_request"),
            ReasonForNoIdentity::NotProvidedByServiceDiscovery => {
                f.pad("not_provided_by_service_discovery")
            }
            ReasonForNoIdentity::Loopback => f.pad("loopback"),
            ReasonForNoIdentity::NotConfigured => f.pad("not_configured"),
            ReasonForNoIdentity::NotImplementedForTap => f.pad("not_implemented_for_tap"),
            ReasonForNoIdentity::NotImplementedForMetrics => f.pad("not_implemented_for_metrics"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Error {
    Io(PathBuf, Option<i32>),
    FailedToParseTrustAnchors(Option<webpki::Error>),
    EndEntityCertIsNotValid(rustls::TLSError),
    InvalidPrivateKey(ring::error::KeyRejected),
}

#[cfg(test)]
pub mod test_util {
    use super::*;
    use std::path::PathBuf;
    use transport::identity::Name;
    use Conditional;

    pub struct Strings {
        pub identity: &'static str,
        pub trust_anchors: &'static str,
        pub end_entity_cert: &'static str,
        pub private_key: &'static str,
    }

    pub static FOO_NS1: Strings = Strings {
        identity: "foo.deployment.ns1.linkerd-managed.linkerd.svc.cluster.local",
        trust_anchors: "ca1.pem",
        end_entity_cert: "foo-ns1-ca1.crt",
        private_key: "foo-ns1-ca1.p8",
    };

    pub static BAR_NS1: Strings = Strings {
        identity: "bar.deployment.ns1.linkerd-managed.linkerd.svc.cluster.local",
        trust_anchors: "ca1.pem",
        end_entity_cert: "bar-ns1-ca1.crt",
        private_key: "bar-ns1-ca1.p8",
    };

    impl Strings {
        pub fn to_settings(&self) -> CommonSettings {
            let dir = PathBuf::from("src/transport/tls/testdata");
            CommonSettings {
                local_identity: Identity::from_sni_hostname(self.identity.as_bytes()).unwrap(),
                controller_identity: Conditional::None(ReasonForNoIdentity::NotConfigured),
                trust_anchors: dir.join(self.trust_anchors),
                end_entity_cert: dir.join(self.end_entity_cert),
                private_key: dir.join(self.private_key),
            }
        }

        // Returns a `ConnectionConfig<ClientConfigWatch>` preloaded with a
        // valid client TLS configuration.
        pub fn client(&self, server_identity: Identity) -> ConnectionConfig<ClientConfigWatch> {
            let settings = self.to_settings();
            let mut config_watch = ConfigWatch::new(Conditional::Some(settings.clone()));
            let common = CommonConfig::load_from_disk(&settings).unwrap();
            config_watch
                .client_store
                .store(Conditional::Some(ClientConfig::from(&common)))
                .unwrap();
            ConnectionConfig {
                server_identity: server_identity,
                config: config_watch.client,
            }
        }

        // Returns a `ConnectionConfig<ServerConfigWatch>` preloaded with a
        // valid server TLS configuration.
        pub fn server(&self) -> ConnectionConfig<ServerConfigWatch> {
            let settings = self.to_settings();
            let mut config_watch = ConfigWatch::new(Conditional::Some(settings.clone()));
            let common = CommonConfig::load_from_disk(&settings).unwrap();
            config_watch
                .server_store
                .store(ServerConfig::from(&common))
                .unwrap();
            let config = match config_watch.server {
                Conditional::Some(watch) => watch,
                Conditional::None(_) => unreachable!(),
            };
            ConnectionConfig {
                server_identity: settings.local_identity,
                config,
            }
        }
    }
}
