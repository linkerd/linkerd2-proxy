#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

use linkerd_dns_name::NameRef;
pub use linkerd_dns_name::{InvalidName, Name, Suffix};
use linkerd_error::Error;
use std::{fmt, net};
use thiserror::Error;
use tokio::time::{self, Instant};
use tracing::{debug, trace};
use trust_dns_resolver::{
    config::ResolverConfig, proto::rr::rdata, system_conf, AsyncResolver, TokioAsyncResolver,
};
pub use trust_dns_resolver::{
    config::ResolverOpts,
    error::{ResolveError, ResolveErrorKind},
};

#[derive(Clone)]
pub struct Resolver {
    dns: TokioAsyncResolver,
}

pub trait ConfigureResolver {
    fn configure_resolver(&self, _: &mut ResolverOpts);
}

#[derive(Debug, Clone, Error)]
#[error("invalid SRV record {:?}", self.0)]
struct InvalidSrv(rdata::SRV);

impl Resolver {
    /// Construct a new `Resolver` from environment variables and system
    /// configuration.
    ///
    /// # Returns
    ///
    /// Either a new `Resolver` or an error if the system configuration
    /// could not be parsed.
    ///
    /// TODO: This should be infallible like it is in the `domain` crate.
    pub fn from_system_config_with<C: ConfigureResolver>(c: &C) -> Result<Self, ResolveError> {
        let (config, mut opts) = system_conf::read_system_conf()?;
        c.configure_resolver(&mut opts);
        trace!("DNS config: {:?}", &config);
        trace!("DNS opts: {:?}", &opts);
        Ok(Self::new(config, opts))
    }

    pub fn new(config: ResolverConfig, mut opts: ResolverOpts) -> Self {
        // Disable Trust-DNS's caching.
        opts.cache_size = 0;
        // This function is synchronous, but needs to be called within the Tokio
        // 0.2 runtime context, since it gets a handle.
        let dns = AsyncResolver::tokio(config, opts).expect("system DNS config must be valid");
        Resolver { dns }
    }

    /// Resolves a name to a set of addresses, preferring SRV records to normal A
    /// record lookups.
    pub async fn resolve_addrs(
        &self,
        name: NameRef<'_>,
        default_port: u16,
    ) -> Result<(Vec<net::SocketAddr>, time::Sleep), Error> {
        match self.resolve_srv(name).await {
            Ok(res) => Ok(res),
            Err(e) if e.is::<InvalidSrv>() => {
                let (ips, delay) = self.resolve_a(name).await?;
                let addrs = ips
                    .into_iter()
                    .map(|ip| net::SocketAddr::new(ip, default_port))
                    .collect();
                Ok((addrs, delay))
            }
            Err(e) => Err(e),
        }
    }

    async fn resolve_a(
        &self,
        name: NameRef<'_>,
    ) -> Result<(Vec<net::IpAddr>, time::Sleep), ResolveError> {
        debug!(%name, "resolve_a");
        let lookup = self.dns.lookup_ip(name.as_str()).await?;
        let valid_until = Instant::from_std(lookup.valid_until());
        let ips = lookup.iter().collect::<Vec<_>>();
        Ok((ips, time::sleep_until(valid_until)))
    }

    async fn resolve_srv(
        &self,
        name: NameRef<'_>,
    ) -> Result<(Vec<net::SocketAddr>, time::Sleep), Error> {
        debug!(%name, "resolve_srv");
        let srv = self.dns.srv_lookup(name.as_str()).await?;

        let valid_until = Instant::from_std(srv.as_lookup().valid_until());
        let addrs = srv
            .into_iter()
            .map(Self::srv_to_socket_addr)
            .collect::<Result<_, InvalidSrv>>()?;
        debug!(ttl = ?valid_until - time::Instant::now(), ?addrs);

        Ok((addrs, time::sleep_until(valid_until)))
    }

    // XXX We need to convert the SRV records to an IP addr manually,
    // because of: https://github.com/bluejekyll/trust-dns/issues/872
    // Here we rely in on the fact that the first label of the SRV
    // record's target will be the ip of the pod delimited by dashes
    // instead of dots. We can alternatively do another lookup
    // on the pod's DNS but it seems unnecessary since the pod's
    // ip is in the target of the SRV record.
    fn srv_to_socket_addr(srv: rdata::SRV) -> Result<net::SocketAddr, InvalidSrv> {
        if let Some(first_label) = srv.target().iter().next() {
            if let Ok(utf8) = std::str::from_utf8(first_label) {
                if let Ok(ip) = utf8.replace("-", ".").parse::<std::net::IpAddr>() {
                    return Ok(net::SocketAddr::new(ip, srv.port()));
                }
            }
        }
        Err(InvalidSrv(srv))
    }
}

/// Note: `AsyncResolver` does not implement `Debug`, so we must manually
///       implement this.
impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("resolver", &"...")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Name, Suffix};
    use std::str::FromStr;

    #[test]
    fn test_dns_name_parsing() {
        // Make sure `dns::Name`'s validation isn't too strict. It is
        // implemented in terms of `webpki::DNSName` which has many more tests
        // at https://github.com/briansmith/webpki/blob/master/tests/dns_name_tests.rs.

        struct Case {
            input: &'static str,
            output: &'static str,
        }

        static VALID: &[Case] = &[
            // Almost all digits and dots, similar to IPv4 addresses.
            Case {
                input: "1.2.3.x",
                output: "1.2.3.x",
            },
            Case {
                input: "1.2.3.x",
                output: "1.2.3.x",
            },
            Case {
                input: "1.2.3.4A",
                output: "1.2.3.4a",
            },
            Case {
                input: "a.b.c.d",
                output: "a.b.c.d",
            },
            // Uppercase letters in labels
            Case {
                input: "A.b.c.d",
                output: "a.b.c.d",
            },
            Case {
                input: "a.mIddle.c",
                output: "a.middle.c",
            },
            Case {
                input: "a.b.c.D",
                output: "a.b.c.d",
            },
            // Absolute
            Case {
                input: "a.b.c.d.",
                output: "a.b.c.d.",
            },
        ];

        for case in VALID {
            let name = Name::from_str(case.input);
            assert_eq!(name.as_deref(), Ok(case.output));
        }

        static INVALID: &[&str] = &[
            // These are not in the "preferred name syntax" as defined by
            // https://tools.ietf.org/html/rfc1123#section-2.1. In particular
            // the last label only has digits.
            "1.2.3.4", "a.1.2.3", "1.2.x.3",
        ];

        for case in INVALID {
            assert!(Name::from_str(case).is_err());
        }
    }

    #[test]
    fn suffix_valid() {
        for (name, suffix) in &[
            ("a", "."),
            ("a.", "."),
            ("a.b", "."),
            ("a.b.", "."),
            ("b.c", "b.c"),
            ("b.c", "b.c"),
            ("a.b.c", "b.c"),
            ("a.b.c", "b.c."),
            ("a.b.c.", "b.c"),
            ("hacker.example.com", "example.com"),
        ] {
            let n = Name::from_str(name).unwrap();
            let s = Suffix::from_str(suffix).unwrap();
            assert!(s.contains(&n), "{} should contain {}", suffix, name);
        }
    }

    #[test]
    fn suffix_invalid() {
        for (name, suffix) in &[
            ("a", "b"),
            ("b", "a.b"),
            ("b.a", "b"),
            ("hackerexample.com", "example.com"),
        ] {
            let n = Name::from_str(name).unwrap();
            let s = Suffix::from_str(suffix).unwrap();
            assert!(!s.contains(&n), "{} should not contain {}", suffix, name);
        }

        assert!(Suffix::from_str("").is_err(), "suffix must not be empty");
    }
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use super::*;

    pub struct FuzzConfig {}

    // Empty config resolver that we can use.
    impl ConfigureResolver for FuzzConfig {
        fn configure_resolver(&self, _opts: &mut ResolverOpts) {}
    }

    // Test the resolvers do not panic unexpectedly.
    pub async fn fuzz_entry(fuzz_data: &str) {
        if let Ok(name) = fuzz_data.parse::<Name>() {
            let fcon = FuzzConfig {};
            let resolver = Resolver::from_system_config_with(&fcon).unwrap();
            let _w = resolver.resolve_a(name.as_ref()).await;
            let _w2 = resolver.resolve_srv(name.as_ref()).await;
        }
    }
}
