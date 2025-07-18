#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use hickory_resolver::config::ResolverOpts;
use hickory_resolver::{config::ResolverConfig, proto::rr::rdata, system_conf, TokioResolver};
use linkerd_dns_name::NameRef;
pub use linkerd_dns_name::{InvalidName, Name, Suffix};
use prometheus_client::metrics::counter::Counter;
use std::{fmt, net};
use thiserror::Error;
use tokio::time::{self, Instant};
use tracing::{debug, trace};

#[derive(Clone)]
pub struct Resolver {
    dns: TokioResolver,
    metrics: Option<Metrics>,
}

pub trait ConfigureResolver {
    fn configure_resolver(&self, _: &mut ResolverOpts);
}

#[derive(Clone)]
pub struct Metrics {
    /// A [`Counter`] tracking the number of A/AAAA records successfully resolved.
    pub a_records_resolved: Counter,
    /// A [`Counter`] tracking the number of A/AAAA records not found.
    pub a_records_not_found: Counter,
    /// A [`Counter`] tracking the number of SRV records successfully resolved.
    pub srv_records_resolved: Counter,
    /// A [`Counter`] tracking the number of SRV records not found.
    pub srv_records_not_found: Counter,
}

#[derive(Debug, Clone, Error)]
#[error("invalid SRV record {:?}", self.0)]
struct InvalidSrv(rdata::SRV);

#[derive(Debug, Error)]
#[error("failed to resolve A record: {0}")]
struct ARecordError(#[from] hickory_resolver::ResolveError);

#[derive(Debug, Error)]
enum SrvRecordError {
    #[error("{0}")]
    Invalid(#[from] InvalidSrv),
    #[error("failed to resolve SRV record: {0}")]
    Resolve(#[from] hickory_resolver::ResolveError),
}

#[derive(Debug, Error)]
#[error("failed SRV and A record lookups: {srv_error}; {a_error}")]
pub struct ResolveError {
    #[source]
    a_error: ARecordError,
    srv_error: SrvRecordError,
}

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
    pub fn from_system_config_with<C: ConfigureResolver>(c: &C) -> std::io::Result<Self> {
        let (config, mut opts) = system_conf::read_system_conf()?;
        c.configure_resolver(&mut opts);
        trace!("DNS config: {:?}", &config);
        trace!("DNS opts: {:?}", &opts);
        Ok(Self::new(config, opts))
    }

    pub fn new(config: ResolverConfig, mut opts: ResolverOpts) -> Self {
        // Disable Hickory-resolver's caching.
        opts.cache_size = 0;
        // This function is synchronous, but needs to be called within the Tokio
        // 0.2 runtime context, since it gets a handle.
        let provider = hickory_resolver::name_server::TokioConnectionProvider::default();
        let mut builder = hickory_resolver::Resolver::builder_with_config(config, provider);
        *builder.options_mut() = opts;
        let dns = builder.build();
        /* TODO(kate): this can be used if/when hickory-dns/hickory-dns#2877 is released.
        let dns = hickory_resolver::Resolver::builder_with_config(config, provider)
            .with_options(opts)
            .build();
        */

        Resolver { dns, metrics: None }
    }

    /// Installs a counter tracking the number of A/AAAA records resolved.
    pub fn with_metrics(self, metrics: Metrics) -> Self {
        Self {
            metrics: Some(metrics),
            ..self
        }
    }

    /// Resolves a name to a set of addresses, preferring SRV records to normal A/AAAA
    /// record lookups.
    pub async fn resolve_addrs(
        &self,
        name: NameRef<'_>,
        default_port: u16,
    ) -> Result<(Vec<net::SocketAddr>, Instant), ResolveError> {
        match self.resolve_srv(name).await {
            Ok(res) => {
                self.metrics.as_ref().map(Metrics::inc_srv_records_resolved);
                Ok(res)
            }
            Err(srv_error) => {
                // If the SRV lookup failed for any reason, fall back to A/AAAA
                // record resolution.
                debug!(srv.error = %srv_error, "Falling back to A/AAAA record lookup");
                self.metrics
                    .as_ref()
                    .map(Metrics::inc_srv_records_not_found);
                let (ips, delay) = match self.resolve_a_or_aaaa(name).await {
                    Ok(res) => res,
                    Err(a_error) => {
                        self.metrics.as_ref().map(Metrics::inc_a_records_not_found);
                        return Err(ResolveError { a_error, srv_error });
                    }
                };
                self.metrics.as_ref().map(Metrics::inc_a_records_resolved);
                let addrs = ips
                    .into_iter()
                    .map(|ip| net::SocketAddr::new(ip, default_port))
                    .collect();
                Ok((addrs, delay))
            }
        }
    }

    async fn resolve_a_or_aaaa(
        &self,
        name: NameRef<'_>,
    ) -> Result<(Vec<net::IpAddr>, Instant), ARecordError> {
        debug!(%name, "Resolving an A/AAAA record");
        let lookup = self.dns.lookup_ip(name.as_str()).await?;
        let ips = lookup.iter().collect::<Vec<_>>();
        let valid_until = Instant::from_std(lookup.valid_until());
        Ok((ips, valid_until))
    }

    async fn resolve_srv(
        &self,
        name: NameRef<'_>,
    ) -> Result<(Vec<net::SocketAddr>, Instant), SrvRecordError> {
        debug!(%name, "Resolving a SRV record");
        let srv = self.dns.srv_lookup(name.as_str()).await?;

        let valid_until = Instant::from_std(srv.as_lookup().valid_until());
        let addrs = srv
            .into_iter()
            .map(Self::srv_to_socket_addr)
            .collect::<Result<_, InvalidSrv>>()?;
        debug!(ttl = ?valid_until - Instant::now(), ?addrs);

        Ok((addrs, valid_until))
    }

    // XXX We need to convert the SRV records to an IP addr manually,
    // because of: https://github.com/hickory-dns/hickory-dns/issues/872
    // Here we rely in on the fact that the first label of the SRV
    // record's target will be the ip of the pod delimited by dashes
    // instead of dots/colons. We can alternatively do another lookup
    // on the pod's DNS but it seems unnecessary since the pod's
    // ip is in the target of the SRV record.
    fn srv_to_socket_addr(srv: rdata::SRV) -> Result<net::SocketAddr, InvalidSrv> {
        if let Some(first_label) = srv.target().iter().next() {
            if let Ok(utf8) = std::str::from_utf8(first_label) {
                let mut res = utf8.replace('-', ".").parse::<std::net::IpAddr>();
                if res.is_err() {
                    res = utf8.replace('-', ":").parse::<std::net::IpAddr>();
                }
                if let Ok(ip) = res {
                    return Ok(net::SocketAddr::new(ip, srv.port()));
                }
            }
        }
        Err(InvalidSrv(srv))
    }
}

/// Note: `hickory_resolver::Resolver` does not implement `Debug`, so we must manually
///       implement this.
impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("resolver", &"...")
            .finish()
    }
}

// === impl ResolveError ===

impl ResolveError {
    /// Returns the amount of time that the resolver should wait before
    /// retrying.
    pub fn negative_ttl(&self) -> Option<time::Duration> {
        if let Some(hickory_resolver::proto::ProtoErrorKind::NoRecordsFound {
            negative_ttl: Some(ttl_secs),
            ..
        }) = self
            .a_error
            .0
            .proto()
            .map(hickory_resolver::proto::ProtoError::kind)
        {
            return Some(time::Duration::from_secs(*ttl_secs as u64));
        }

        if let SrvRecordError::Resolve(error) = &self.srv_error {
            if let Some(hickory_resolver::proto::ProtoErrorKind::NoRecordsFound {
                negative_ttl: Some(ttl_secs),
                ..
            }) = error.proto().map(hickory_resolver::proto::ProtoError::kind)
            {
                return Some(time::Duration::from_secs(*ttl_secs as u64));
            }
        }

        None
    }
}

// === impl Metrics ===

impl Metrics {
    fn inc_a_records_resolved(&self) {
        self.a_records_resolved.inc();
    }

    fn inc_a_records_not_found(&self) {
        self.a_records_not_found.inc();
    }

    fn inc_srv_records_resolved(&self) {
        self.srv_records_resolved.inc();
    }

    fn inc_srv_records_not_found(&self) {
        self.srv_records_not_found.inc();
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidSrv, Name, Resolver, SrvRecordError, Suffix};
    use hickory_resolver::proto::rr::{domain, rdata};
    use std::{net, str::FromStr};

    #[test]
    fn test_dns_name_parsing() {
        // Make sure `dns::Name`'s validation isn't too strict. It is
        // implemented in terms of `webpki::DnsName` which has many more tests
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
            assert!(s.contains(&n), "{suffix} should contain {name}");
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
            assert!(!s.contains(&n), "{suffix} should not contain {name}");
        }

        assert!(Suffix::from_str("").is_err(), "suffix must not be empty");
    }

    #[test]
    fn srv_to_socket_addr_invalid() {
        let name = "foobar.linkerd-dst-headless.linkerd.svc.cluster.local.";
        let target = domain::Name::from_str(name).unwrap();
        let srv = rdata::SRV::new(1, 1, 8086, target);
        assert!(Resolver::srv_to_socket_addr(srv).is_err());
    }

    #[test]
    fn srv_to_socket_addr_valid() {
        struct Case {
            input: &'static str,
            output: &'static str,
        }

        for case in &[
            Case {
                input: "10-42-0-15.linkerd-dst-headless.linkerd.svc.cluster.local.",
                output: "10.42.0.15",
            },
            Case {
                input: "2001-0db8-0000-0000-0000-ff00-0042-8329.linkerd-dst-headless.linkerd.svc.cluster.local.",
                output: "2001:0db8:0000:0000:0000:ff00:0042:8329",
            },
            Case {
                input: "2001-0db8--0042-8329.linkerd-dst-headless.linkerd.svc.cluster.local.",
                output: "2001:0db8::0042:8329",
            },
        ] {
            let target = domain::Name::from_str(case.input).unwrap();
            let srv = rdata::SRV::new(1, 1, 8086, target);
            let socket = Resolver::srv_to_socket_addr(srv).unwrap();
            assert_eq!(socket.ip(), net::IpAddr::from_str(case.output).unwrap());
        }
    }

    #[test]
    fn srv_record_reports_cause_correctly() {
        let srv = "foobar.linkerd-dst-headless.linkerd.svc.cluster.local."
            .parse::<hickory_resolver::Name>()
            .map(|name| rdata::SRV::new(1, 1, 8086, name))
            .expect("a valid domain name");

        let error = SrvRecordError::Invalid(InvalidSrv(srv));
        let error: Box<dyn std::error::Error + 'static> = Box::new(error);

        assert!(linkerd_error::is_caused_by::<InvalidSrv>(&*error));
        assert!(linkerd_error::cause_ref::<InvalidSrv>(&*error).is_some());
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
            let _w = resolver.resolve_a_or_aaaa(name.as_ref()).await;
            let _w2 = resolver.resolve_srv(name.as_ref()).await;
        }
    }
}
