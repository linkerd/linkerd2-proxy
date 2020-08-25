#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "512"]
mod refine;

pub use self::refine::{MakeRefine, Refine};
use futures::prelude::*;
pub use linkerd2_dns_name::{InvalidName, Name, Suffix};
use std::{fmt, net};
use tokio::{
    sync::mpsc,
    time::{self, Instant},
};
use tracing::{info_span, trace};
use tracing_futures::Instrument;
use trust_dns_resolver::{
    config::ResolverConfig, lookup_ip::LookupIp, proto::rr::rdata, system_conf, AsyncResolver,
    TokioAsyncResolver,
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

#[derive(Debug, Clone)]
pub enum Error {
    ResolutionFailed(ResolveError),
    InvalidSRVRecord(rdata::SRV),
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
        let dns = AsyncResolver::tokio(config, opts).expect("Resolver must be valid");
        Resolver { dns }
    }

    async fn lookup_ip(&self, name: Name) -> Result<LookupIp, Error> {
        self.dns.lookup_ip(name.as_ref()).err_into::<Error>().await
    }

    // XXX We need to convert the SRV records to an IP addr manually,
    // because of: https://github.com/bluejekyll/trust-dns/issues/872
    // Here we rely in on the fact that the first label of the SRV
    // record's target will be the ip of the pod delimited by dashes
    // instead of dots. We can alternatively do another lookup
    // on the pod's DNS but it seems unnecessary since the pod's
    // ip is in the target of the SRV record.
    fn srv_to_socket_addr(srv: rdata::SRV) -> Result<net::SocketAddr, Error> {
        if let Some(first_label) = srv.target().iter().next() {
            if let Ok(utf8) = std::str::from_utf8(first_label) {
                if let Ok(ip) = utf8.replace("-", ".").parse::<std::net::IpAddr>() {
                    return Ok(net::SocketAddr::new(ip, srv.port()));
                }
            }
        }
        Err(Error::InvalidSRVRecord(srv))
    }

    pub fn resolve_service_addrs(
        &self,
        name: Name,
    ) -> mpsc::Receiver<Result<Vec<net::SocketAddr>, Error>> {
        let dns = self.dns.clone();
        let (mut tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                match dns
                    .srv_lookup(name.as_ref())
                    .instrument(info_span!("srv_lookup", %name))
                    .await
                {
                    Ok(srv_records) => {
                        let valid_until = Instant::from_std(srv_records.as_lookup().valid_until());
                        let update = srv_records
                            .into_iter()
                            .map(Self::srv_to_socket_addr)
                            .collect();
                        if tx.send(update).await.is_err() {
                            return;
                        }
                        time::delay_until(valid_until).await;
                    }
                    Err(e) => match e.kind() {
                        ResolveErrorKind::NoRecordsFound { valid_until, .. } => {
                            if tx.send(Ok(vec![])).await.is_err() {
                                return;
                            }

                            if let Some(expiry) = valid_until {
                                time::delay_until(Instant::from_std(*expiry)).await;
                            }
                        }
                        _ => {
                            if tx.send(Err(e.into())).await.is_err() {
                                return;
                            }
                        }
                    },
                }
            }
        });
        rx
    }

    /// Creates a refining service.
    pub fn into_make_refine(self) -> MakeRefine {
        MakeRefine(self)
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

impl From<ResolveError> for Error {
    fn from(e: ResolveError) -> Self {
        Self::ResolutionFailed(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResolutionFailed(e) => fmt::Display::fmt(e, f),
            Self::InvalidSRVRecord(srv) => write!(f, "Invalid SRV record {:?}", srv),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ResolutionFailed(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Name, Suffix};
    use std::convert::TryFrom;

    #[test]
    fn test_dns_name_parsing() {
        // Stack sure `dns::Name`'s validation isn't too strict. It is
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
            let name = Name::try_from(case.input.as_bytes());
            assert_eq!(name.as_ref().map(|x| x.as_ref()), Ok(case.output));
        }

        static INVALID: &[&str] = &[
            // These are not in the "preferred name syntax" as defined by
            // https://tools.ietf.org/html/rfc1123#section-2.1. In particular
            // the last label only has digits.
            "1.2.3.4", "a.1.2.3", "1.2.x.3",
        ];

        for case in INVALID {
            assert!(Name::try_from(case.as_bytes()).is_err());
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
            let n = Name::try_from((*name).as_bytes()).unwrap();
            let s = Suffix::try_from(*suffix).unwrap();
            assert!(
                s.contains(&n),
                format!("{} should contain {}", suffix, name)
            );
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
            let n = Name::try_from((*name).as_bytes()).unwrap();
            let s = Suffix::try_from(*suffix).unwrap();
            assert!(
                !s.contains(&n),
                format!("{} should not contain {}", suffix, name)
            );
        }

        assert!(Suffix::try_from("").is_err(), "suffix must not be empty");
    }
}
