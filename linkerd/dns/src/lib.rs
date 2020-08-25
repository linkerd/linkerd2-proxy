#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "512"]
mod refine;

pub use self::refine::{MakeRefine, Refine};
use async_stream::try_stream;
use futures::stream::Stream;
pub use linkerd2_dns_name::{InvalidName, Name, Suffix};
use std::future::Future;
use std::pin::Pin;
use std::{fmt, net};
use tokio::sync::{mpsc, oneshot};
use tokio::time;
use tokio::time::Instant;
use tracing::{info_span, trace, Span};
use tracing_futures::Instrument;
pub use trust_dns_resolver::config::ResolverOpts;
pub use trust_dns_resolver::error::{ResolveError, ResolveErrorKind};
use trust_dns_resolver::lookup::SrvLookup;
pub use trust_dns_resolver::lookup_ip::LookupIp;
use trust_dns_resolver::proto::rr::rdata;
use trust_dns_resolver::{config::ResolverConfig, system_conf, AsyncResolver};

#[derive(Clone)]
pub struct Resolver {
    tx: mpsc::UnboundedSender<ResolveRequest>,
}

pub trait ConfigureResolver {
    fn configure_resolver(&self, _: &mut ResolverOpts);
}

#[derive(Debug, Clone)]
pub enum Error {
    ResolutionFailed(ResolveError),
    TaskLost,
    InvalidSRVRecord(rdata::SRV),
}

enum LookupType {
    Ip(oneshot::Sender<Result<LookupIp, ResolveError>>),
    Service(oneshot::Sender<Result<SrvLookup, ResolveError>>),
}

struct ResolveRequest {
    name: Name,
    span: tracing::Span,
    lookup_type: LookupType,
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

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
    pub fn from_system_config_with<C: ConfigureResolver>(
        c: &C,
    ) -> Result<(Self, Task), ResolveError> {
        let (config, mut opts) = system_conf::read_system_conf()?;
        c.configure_resolver(&mut opts);
        trace!("DNS config: {:?}", &config);
        trace!("DNS opts: {:?}", &opts);
        Self::new(config, opts)
    }

    pub fn new(
        config: ResolverConfig,
        mut opts: ResolverOpts,
    ) -> Result<(Self, Task), ResolveError> {
        // Disable Trust-DNS's caching.
        opts.cache_size = 0;

        // XXX(eliza): figure out an appropriate bound for the channel...
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = Box::pin(async move {
            let resolver = match AsyncResolver::tokio(config, opts) {
                Ok(resolver) => resolver,
                Err(e) => unreachable!("constructing resolver should not fail: {}", e),
            };
            while let Some(ResolveRequest {
                name,
                span,
                lookup_type,
            }) = rx.recv().await
            {
                match lookup_type {
                    LookupType::Ip(result_tx) => {
                        let resolver = resolver.clone();
                        tokio::spawn(
                            async move {
                                let res = resolver.lookup_ip(name.as_ref()).await;
                                if result_tx.send(res).is_err() {
                                    tracing::debug!("resolution canceled");
                                }
                            }
                            .instrument(span),
                        );
                    }
                    LookupType::Service(result_tx) => {
                        let resolver = resolver.clone();
                        tokio::spawn(
                            async move {
                                let res = resolver.srv_lookup(name.as_ref()).await;
                                if result_tx.send(res).is_err() {
                                    tracing::debug!("resolution canceled");
                                }
                            }
                            .instrument(span),
                        );
                    }
                }
            }
            tracing::debug!("all resolver handles dropped; terminating.");
        });
        Ok((Resolver { tx }, task))
    }

    async fn lookup_ip(&self, name: Name, span: Span) -> Result<LookupIp, Error> {
        let (result_tx, rx) = oneshot::channel();
        self.tx.send(ResolveRequest {
            name,
            span,
            lookup_type: LookupType::Ip(result_tx),
        })?;
        let ips = rx.await??;
        Ok(ips)
    }

    async fn lookup_service(&self, name: Name, span: Span) -> Result<SrvLookup, Error> {
        let (result_tx, rx) = oneshot::channel();
        self.tx.send(ResolveRequest {
            name,
            span,
            lookup_type: LookupType::Service(result_tx),
        })?;
        let srvs = rx.await??;
        Ok(srvs)
    }

    // XXX We need to convert the SRV records to an IP addr manually,
    // because of: https://github.com/bluejekyll/trust-dns/issues/872
    // Here we rely in on the fact that the first label of the SRV
    // record's target will be the ip of the pod delimited by dashes
    // instead of dots. We can alternatively do another lookup
    // on the pod's DNS but it seems unnecessary since the pod's
    // ip is in the target of the SRV record.
    fn srv_to_socket_addr(srv: &rdata::SRV) -> Result<net::SocketAddr, Error> {
        let first_label = srv
            .target()
            .iter()
            .next()
            .ok_or_else(|| Error::InvalidSRVRecord(srv.clone()))?;

        std::str::from_utf8(first_label)
            .expect("should be utf8")
            .replace("-", ".")
            .parse::<std::net::IpAddr>()
            .map(|ip| net::SocketAddr::new(ip, srv.port()))
            .map_err(|_| Error::InvalidSRVRecord(srv.clone()))
    }

    pub fn resolve_service_addrs(
        &self,
        svc: &Name,
    ) -> impl Stream<Item = Result<Vec<net::SocketAddr>, Error>> {
        let resolver = self.clone();
        let svc_name = svc.clone();
        try_stream! {
            loop {
                let span = info_span!("lookup_service", %svc_name);
                match resolver
                    .lookup_service(svc_name.clone(), span.clone())
                    .await
                {
                    Ok(srv_records) => {
                        let addresses =
                            srv_records.iter().map(Self::srv_to_socket_addr).collect()?;
                        yield addresses;
                        let valid_until = Instant::from_std(srv_records.as_lookup().valid_until());
                        time::delay_until(valid_until).await;
                    }
                    Err(Error::ResolutionFailed(err)) => match err.kind() {
                        ResolveErrorKind::NoRecordsFound { valid_until, .. } => {
                            yield vec![];
                            if let Some(ttl) = valid_until {
                                time::delay_until(Instant::from_std(ttl.clone())).await;
                            }
                        }
                        _ => Err(err)?,
                    },
                    Err(err) => Err(err)?,
                }
            }
        }
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

impl<T> From<mpsc::error::SendError<T>> for Error {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        Self::TaskLost
    }
}

impl From<oneshot::error::RecvError> for Error {
    fn from(_: oneshot::error::RecvError) -> Self {
        Self::TaskLost
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
            Self::TaskLost => f.pad("background task terminated unexpectedly"),
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
