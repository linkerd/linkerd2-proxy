mod name;

pub use self::name::{InvalidName, Name};
use crate::logging;
use futures::{prelude::*, try_ready};
use std::convert::TryFrom;
use std::time::Instant;
use std::{fmt, net};
use tracing::trace;
pub use trust_dns_resolver::config::ResolverOpts;
pub use trust_dns_resolver::error::{ResolveError, ResolveErrorKind};
use trust_dns_resolver::{config::ResolverConfig, system_conf, AsyncResolver, BackgroundLookupIp};

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

pub struct IpAddrFuture(logging::ContextualFuture<Ctx, BackgroundLookupIp>);

pub struct RefineFuture(logging::ContextualFuture<Ctx, BackgroundLookupIp>);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Suffix {
    Root, // The `.` suffix.
    Name(Name),
}

struct Ctx(Name);

pub struct Refine {
    pub name: Name,
    pub valid_until: Instant,
}

impl fmt::Display for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "dns={}", self.0)
    }
}

impl fmt::Display for Suffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Suffix::Root => write!(f, "."),
            Suffix::Name(n) => n.fmt(f),
        }
    }
}

impl From<Name> for Suffix {
    fn from(n: Name) -> Self {
        Suffix::Name(n)
    }
}

impl<'s> TryFrom<&'s str> for Suffix {
    type Error = <Name as TryFrom<&'s [u8]>>::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s == "." {
            Ok(Suffix::Root)
        } else {
            Name::try_from(s.as_bytes()).map(|n| n.into())
        }
    }
}

impl Suffix {
    pub fn contains(&self, name: &Name) -> bool {
        match self {
            Suffix::Root => true,
            Suffix::Name(ref sfx) => {
                let name = name.without_trailing_dot();
                let sfx = sfx.without_trailing_dot();
                name.ends_with(sfx) && {
                    name.len() == sfx.len() || {
                        // foo.bar.bah (11)
                        // bar.bah (7)
                        let idx = name.len() - sfx.len();
                        let (hd, _) = name.split_at(idx);
                        hd.ends_with('.')
                    }
                }
            }
        }
    }
}

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
    ) -> Result<(Self, impl Future<Item = (), Error = ()> + Send), ResolveError> {
        let (config, mut opts) = system_conf::read_system_conf()?;
        c.configure_resolver(&mut opts);
        trace!("DNS config: {:?}", &config);
        trace!("DNS opts: {:?}", &opts);
        Ok(Self::new(config, opts))
    }

    /// NOTE: It would be nice to be able to return a named type rather than
    ///       `impl Future` for the background future; it would be called
    ///       `Background` or `ResolverBackground` if that were possible.
    pub fn new(
        config: ResolverConfig,
        mut opts: ResolverOpts,
    ) -> (Self, impl Future<Item = (), Error = ()> + Send) {
        // Disable Trust-DNS's caching.
        opts.cache_size = 0;
        let (resolver, background) = AsyncResolver::new(config, opts);
        let resolver = Resolver { resolver };
        (resolver, background)
    }

    pub fn resolve_one_ip(&self, name: &Name) -> IpAddrFuture {
        let f = self.resolver.lookup_ip(name.as_ref());
        IpAddrFuture(logging::context_future(Ctx(name.clone()), f))
    }

    /// Attempts to refine `name` to a fully-qualified name.
    ///
    /// This method does DNS resolution for `name` and ignores the IP address
    /// result, instead returning the `Name` that was resolved.
    ///
    /// For example, a name like `web` may be refined to `web.example.com.`,
    /// depending on the DNS search path.
    pub fn refine(&self, name: &Name) -> RefineFuture {
        let f = self.resolver.lookup_ip(name.as_ref());
        RefineFuture(logging::context_future(Ctx(name.clone()), f))
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
