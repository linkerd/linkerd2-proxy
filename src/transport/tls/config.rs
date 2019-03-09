use std::{
    self, fmt,
    path::PathBuf,
    sync::Arc,
};

use super::{cert_resolver::CertResolver, rustls, webpki, Identity};
use Conditional;

use futures_watch::Watch;
use ring::signature;

/// Validated configuration common between TLS clients and TLS servers.
#[derive(Debug)]
struct CommonConfig {
    root_cert_store: rustls::RootCertStore,
    cert_resolver: Arc<CertResolver>,
}

/// Validated configuration for TLS servers.
#[derive(Clone)]
pub struct ClientConfig(pub(super) Arc<rustls::ClientConfig>);

/// XXX: `rustls::ClientConfig` doesn't implement `Debug` yet.
impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        f.debug_struct("ClientConfig").finish()
    }
}

/// Validated configuration for TLS servers.
#[derive(Clone)]
pub struct ServerConfig(pub(super) Arc<rustls::ServerConfig>);

/// XXX: `rustls::ServerConfig` doesn't implement `Debug` yet.
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        f.debug_struct("ServerConfig").finish()
    }
}

pub type ClientConfigWatch = Watch<Conditional<ClientConfig, ReasonForNoTls>>;
pub type ServerConfigWatch = Watch<ServerConfig>;

/// The configuration in effect for a client (`ClientConfig`) or server
/// (`ServerConfig`) TLS connection.
#[derive(Clone, Debug)]
pub struct ConnectionConfig<C>
where
    C: Clone,
{
    pub server_identity: Identity,
    pub config: C,
}

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

pub type ConditionalConnectionConfig<C> = Conditional<ConnectionConfig<C>, ReasonForNoTls>;
pub type ConditionalClientConfig = Conditional<ClientConfig, ReasonForNoTls>;

#[derive(Clone, Debug)]
pub enum Error {
    Io(PathBuf, Option<i32>),
    FailedToParseTrustAnchors(Option<webpki::Error>),
    EndEntityCertIsNotValid(rustls::TLSError),
    InvalidPrivateKey(ring::error::KeyRejected),
}

impl CommonConfig {
    /// Loads a configuration from the given files and validates it. If an
    /// error is returned then the caller should try again after the files are
    /// updated.
    ///
    /// In a valid configuration, all the files need to be in sync with each
    /// other. For example, the private key file must contain the private
    /// key for the end-entity certificate, and the end-entity certificate
    /// must be issued by the CA represented by a certificate in the
    /// trust anchors file. Since filesystem operations are not atomic, we
    /// need to check for this consistency.
    fn validate(
        root_cert_store: rustls::RootCertStore,
        cert_chain: Vec<rustls::Certificate>,
        private_key: signature::EcdsaKeyPair,
        local_identity: Identity,
    ) -> Result<Self, Error> {
        // Ensure the certificate is valid for the services we terminate for
        // TLS. This assumes that server cert validation does the same or
        // more validation than client cert validation.
        //
        // XXX: Rustls currently only provides access to a
        // `ServerCertVerifier` through
        // `rustls::ClientConfig::get_verifier()`.
        //
        // XXX: Once `rustls::ServerCertVerified` is exposed in Rustls's
        // safe API, use it to pass proof to CertResolver::new....
        //
        // TODO: Restrict accepted signatutre algorithms.
        static NO_OCSP: &'static [u8] = &[];
        rustls::ClientConfig::new()
            .get_verifier()
            .verify_server_cert(
                &root_cert_store,
                &cert_chain,
                local_identity.as_dns_name_ref(),
                NO_OCSP,
            )
            .map_err(|err| {
                error!(
                    "validating certificate failed for {:?}: {}",
                    local_identity, err
                );
                Error::EndEntityCertIsNotValid(err)
            })?;

        // `CertResolver::new` is responsible for verifying that the
        // private key is the right one for the certificate.
        let cert_resolver = CertResolver::new(cert_chain, private_key);
        Ok(Self {
            root_cert_store,
            cert_resolver: Arc::new(cert_resolver),
        })
    }

    fn empty() -> Self {
        Self {
            root_cert_store: rustls::RootCertStore::empty(),
            cert_resolver: Arc::new(CertResolver::empty()),
        }
    }
}

impl ClientConfig {
    fn from(common: &CommonConfig) -> Self {
        let mut config = rustls::ClientConfig::new();
        set_common_settings(&mut config.versions);

        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        // TODO: Change Rustls's API to Avoid needing to clone `root_cert_store`.
        config.root_store = common.root_cert_store.clone();

        // Disable session resumption for the time-being until resumption is
        // more tested.
        config.enable_tickets = false;

        // Enable client authentication if and only if we were configured for
        // it.
        config.client_auth_cert_resolver = common.cert_resolver.clone();

        ClientConfig(Arc::new(config))
    }

    /// Some tests aren't set up to do TLS yet, but we require a
    /// `ClientConfigWatch`. We can't use `#[cfg(test)]` here because the
    /// benchmarks use this.
    pub fn no_tls() -> ClientConfigWatch {
        let (watch, _) = Watch::new(Conditional::None(ReasonForNoTls::Disabled));
        watch
    }
}

impl ServerConfig {
    fn from(common: &CommonConfig) -> Self {
        // Ask TLS clients for a certificate and accept any certificate issued
        // by our trusted CA(s).
        //
        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        //
        // TODO: Change Rustls's API to Avoid needing to clone `root_cert_store`.
        let client_cert_verifier =
            rustls::AllowAnyAuthenticatedClient::new(common.root_cert_store.clone());

        let mut config = rustls::ServerConfig::new(client_cert_verifier);
        set_common_settings(&mut config.versions);
        config.cert_resolver = common.cert_resolver.clone();
        ServerConfig(Arc::new(config))
    }
}

fn set_common_settings(versions: &mut Vec<rustls::ProtocolVersion>) {
    // Only enable TLS 1.2 until TLS 1.3 is stable.
    *versions = vec![rustls::ProtocolVersion::TLSv1_2]

    // XXX: Rustls doesn't provide a good way to customize the cipher suite
    // support, so just use its defaults, which are still pretty good.
    // TODO: Expand Rustls's API to allow us to clearly whitelist the cipher
    // suites we want to enable.
}

// Keep these in sync.
pub(super) static SIGNATURE_ALG_RING_SIGNING: &signature::EcdsaSigningAlgorithm =
    &signature::ECDSA_P256_SHA256_ASN1_SIGNING;
pub(super) const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
    rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
pub(super) const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
    rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;

#[cfg(test)]
pub mod test_util {
    use super::*;
    use std::path::PathBuf;
    use transport::tls::Identity;
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

#[cfg(test)]
mod tests {
    use super::{test_util::*, CommonConfig, Error};
    use transport::tls::{ClientConfig, ServerConfig};

    #[test]
    fn can_construct_client_and_server_config_from_valid_settings() {
        let settings = FOO_NS1.to_settings();
        let common = CommonConfig::load_from_disk(&settings).unwrap();
        let _: ClientConfig = ClientConfig::from(&common); // infallible
        let _: ServerConfig = ServerConfig::from(&common); // infallible
    }

    #[test]
    fn recognize_ca_did_not_issue_cert() {
        let settings = Strings {
            trust_anchors: "ca2.pem",
            ..FOO_NS1
        }
        .to_settings();
        match CommonConfig::load_from_disk(&settings) {
            Err(Error::EndEntityCertIsNotValid(_)) => (),
            r => unreachable!("CommonConfig::load_from_disk returned {:?}", r),
        }
    }

    #[test]
    fn recognize_cert_is_not_valid_for_identity() {
        let settings = Strings {
            end_entity_cert: "bar-ns1-ca1.crt",
            private_key: "bar-ns1-ca1.p8",
            ..FOO_NS1
        }
        .to_settings();
        match CommonConfig::load_from_disk(&settings) {
            Err(Error::EndEntityCertIsNotValid(_)) => (),
            r => unreachable!("CommonConfig::load_from_disk returned {:?}", r),
        }
    }

    // XXX: The check that this tests hasn't been implemented yet.
    #[test]
    #[should_panic]
    fn recognize_private_key_is_not_valid_for_cert() {
        let settings = Strings {
            private_key: "bar-ns1-ca1.p8",
            ..FOO_NS1
        }
        .to_settings();
        match CommonConfig::load_from_disk(&settings) {
            Err(_) => (), // // TODO: Err(Error::InvalidPrivateKey) > (),
            r => unreachable!("CommonConfig::load_from_disk returned {:?}", r),
        }
    }
}
