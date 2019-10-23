#![deny(warnings, rust_2018_idioms)]

use futures::{prelude::*, try_ready};
pub use linkerd2_dns_name::{InvalidName, Name, Suffix};
use std::convert::TryFrom;
use std::time::Instant;
use std::{fmt, net};
use tracing::{info_span, trace};
use tracing_futures::Instrument;
pub use trust_dns_resolver::config::ResolverOpts;
pub use trust_dns_resolver::error::{ResolveError, ResolveErrorKind};
use trust_dns_resolver::lookup_ip::LookupIp;
use trust_dns_resolver::{config::ResolverConfig, system_conf, AsyncResolver};

#[derive(Clone)]
pub struct Resolver {
    resolver: AsyncResolver,
}

pub trait ConfigureResolver {
    fn configure_resolver(&self, _: &mut ResolverOpts);
}

#[derive(Debug)]
pub enum Error {
    NoAddressesFound,
    ResolutionFailed(ResolveError),
}

pub struct IpAddrFuture(Box<dyn Future<Item = LookupIp, Error = ResolveError> + Send + 'static>);

pub struct RefineFuture(Box<dyn Future<Item = LookupIp, Error = ResolveError> + Send + 'static>);

pub struct Refine {
    pub name: Name,
    pub valid_until: Instant,
}

pub type Task = Box<dyn Future<Item = (), Error = ()> + Send + 'static>;

impl Resolver {
    /// Construct a new `Resolver` from environment variables and system
    /// configuration.
    ///
    /// # Returns
    ///
    /// Either a tuple containing a new `Resolver` and the background task to
    /// drive that resolver's futures, or an error if the system configuration
    /// could not be parsed.
    ///
    /// TODO: This should be infallible like it is in the `domain` crate.
    pub fn from_system_config_with<C: ConfigureResolver>(
        c: &C,
    ) -> Result<(Self, Task), ResolveError> {
        let (config, mut opts) = system_conf::read_system_conf()?;
        c.configure_resolver(&mut opts);
        trace!("DNS config: {:?}", &config);
        trace!("DNS opts: {:?}", &opts);
        Ok(Self::new(config, opts))
    }

    pub fn new(config: ResolverConfig, mut opts: ResolverOpts) -> (Self, Task) {
        // Disable Trust-DNS's caching.
        opts.cache_size = 0;
        let (resolver, task) = AsyncResolver::new(config, opts);
        let resolver = Resolver { resolver };
        (resolver, Box::new(task))
    }

    pub fn resolve_one_ip(&self, name: &Name) -> IpAddrFuture {
        let name = name.clone();
        let f = self
            .resolver
            .lookup_ip(name.as_ref())
            .instrument(info_span!("resolve_one_ip", %name));
        IpAddrFuture(Box::new(f))
    }

    /// Attempts to refine `name` to a fully-qualified name.
    ///
    /// This method does DNS resolution for `name` and ignores the IP address
    /// result, instead returning the `Name` that was resolved.
    ///
    /// For example, a name like `web` may be refined to `web.example.com.`,
    /// depending on the DNS search path.
    pub fn refine(&self, name: &Name) -> RefineFuture {
        let name = name.clone();
        let f = self
            .resolver
            .lookup_ip(name.as_ref())
            .instrument(info_span!("refine", %name));
        RefineFuture(Box::new(f))
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

impl Future for IpAddrFuture {
    type Item = net::IpAddr;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ips = try_ready!(self.0.poll().map_err(Error::ResolutionFailed));
        ips.iter()
            .next()
            .map(Async::Ready)
            .ok_or_else(|| Error::NoAddressesFound)
    }
}

impl Future for RefineFuture {
    type Item = Refine;
    type Error = ResolveError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let lookup = try_ready!(self.0.poll());
        let valid_until = lookup.valid_until();

        let n = lookup.query().name();
        let name = Name::try_from(n.to_ascii().as_bytes())
            .expect("Name returned from resolver must be valid");

        let refine = Refine { name, valid_until };
        Ok(Async::Ready(refine))
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
