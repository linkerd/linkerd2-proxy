#![deny(warnings, rust_2018_idioms)]

mod refine;

pub use self::refine::{MakeRefine, Refine};
pub use linkerd2_dns_name::{InvalidName, Name, Suffix};
use std::future::Future;
use std::pin::Pin;
use std::{fmt, net};
use tracing::{info_span, trace};
use tracing_futures::Instrument;
use trust_dns_resolver::{
    config::ResolverConfig, lookup_ip::LookupIp, system_conf, AsyncResolver, TokioAsyncResolver,
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

#[derive(Debug)]
pub enum Error {
    NoAddressesFound,
    ResolutionFailed(ResolveError),
}

pub type IpAddrFuture = Pin<Box<dyn Future<Output = Result<net::IpAddr, Error>> + Send + 'static>>;

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
        let lookup = self.dns.lookup_ip(name.as_ref()).await?;
        Ok(lookup)
    }

    pub fn resolve_one_ip(
        &self,
        name: &Name,
    ) -> Pin<Box<dyn Future<Output = Result<net::IpAddr, Error>> + Send + 'static>> {
        let name = name.clone();
        let resolver = self.clone();
        Box::pin(async move {
            let span = info_span!("resolve_one_ip", %name);
            let ips = resolver.lookup_ip(name).instrument(span).await?;
            ips.iter().next().ok_or_else(|| Error::NoAddressesFound)
        })
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
            Self::NoAddressesFound => f.pad("no addresses found"),
            Self::ResolutionFailed(e) => fmt::Display::fmt(e, f),
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
