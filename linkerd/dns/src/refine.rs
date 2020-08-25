use futures::{future, ready};

use super::{Error, Resolver};
use linkerd2_dns_name::Name;
use linkerd2_stack::NewService;
use std::convert::TryFrom;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;
use tracing_futures::Instrument;
use trust_dns_resolver::lookup_ip::LookupIp;

/// A `MakeService` that produces a `Refine` for a given name.
#[derive(Clone)]
pub struct MakeRefine(pub(super) Resolver);

/// A `Service` that produces the most recent result if one is known.
pub struct Refine {
    resolver: Resolver,
    name: Name,
    state: State,
}

enum State {
    Init,
    Pending(Pin<Box<dyn Future<Output = Result<LookupIp, Error>> + Send + 'static>>),
    Refined {
        name: Name,
        ips: Vec<IpAddr>,
        index: usize,
        valid_until: Instant,
    },
}

impl NewService<Name> for MakeRefine {
    type Service = Refine;

    fn new_service(&self, name: Name) -> Self::Service {
        Refine {
            name,
            state: State::Init,
            resolver: self.0.clone(),
        }
    }
}

impl tower::Service<()> for Refine {
    type Response = (Name, IpAddr);
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                State::Init => {
                    let name = self.name.clone();
                    let dns = self.resolver.clone();
                    State::Pending(Box::pin(async move {
                        dns.lookup_ip(name).in_current_span().await
                    }))
                }
                State::Pending(ref mut fut) => {
                    let lookup = ready!(fut.as_mut().poll(cx))?;
                    let valid_until = lookup.valid_until();
                    let n = lookup.query().name();
                    let name = Name::try_from(n.to_ascii().as_bytes())
                        .expect("Name returned from resolver must be valid");
                    let ips = lookup.iter().collect::<Vec<_>>();
                    assert!(ips.len() > 0);
                    State::Refined {
                        name,
                        ips,
                        index: 0,
                        valid_until,
                    }
                }
                State::Refined { valid_until, .. } => {
                    if Instant::now() < valid_until {
                        return Poll::Ready(Ok(()));
                    }
                    State::Init
                }
            }
        }
    }

    fn call(&mut self, _: ()) -> Self::Future {
        if let State::Refined {
            ref name,
            ref ips,
            ref mut index,
            ..
        } = self.state
        {
            let ip = ips[*index % ips.len()];
            *index += 1;
            return future::ok((name.clone(), ip));
        }

        unreachable!("called before ready");
    }
}
