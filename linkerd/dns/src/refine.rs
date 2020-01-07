use futures::{future, try_ready, Future, Poll};
use linkerd2_dns_name::Name;
use linkerd2_stack::NewService;
use std::convert::TryFrom;
use std::time::Instant;
use trust_dns_resolver::{error::ResolveError, lookup_ip::LookupIp, AsyncResolver};

#[derive(Clone)]
pub struct MakeRefine(pub(super) AsyncResolver);

pub struct Refine {
    resolver: AsyncResolver,
    name: Name,
    state: State,
}

enum State {
    Init,
    Pending(Box<dyn Future<Item = LookupIp, Error = ResolveError> + Send + 'static>),
    Refined { name: Name, valid_until: Instant },
}

#[derive(Debug)]
pub struct RefineError(ResolveError);

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
    type Response = Name;
    type Error = RefineError;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            self.state = match self.state {
                State::Init => {
                    let fut = self.resolver.lookup_ip(self.name.as_ref());
                    State::Pending(Box::new(fut))
                }
                State::Pending(ref mut fut) => {
                    let lookup = try_ready!(fut.poll().map_err(RefineError));
                    let valid_until = lookup.valid_until();
                    let n = lookup.query().name();
                    let name = Name::try_from(n.to_ascii().as_bytes())
                        .expect("Name returned from resolver must be valid");
                    State::Refined { name, valid_until }
                }
                State::Refined { valid_until, .. } => {
                    if Instant::now() < valid_until {
                        return Ok(().into());
                    }
                    State::Init
                }
            }
        }
    }

    fn call(&mut self, _: ()) -> Self::Future {
        if let State::Refined { ref name, .. } = self.state {
            return future::ok(name.clone());
        }

        unreachable!("called before ready");
    }
}

impl std::fmt::Display for RefineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for RefineError {}
