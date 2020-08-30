use super::OverrideDestination;
use crate::Target;
use futures::{future, TryFutureExt};
use linkerd2_addr::Addr;
use linkerd2_error::Error;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;
use std::task::{Context, Poll};
use tokio::sync::watch;
pub use tokio::sync::watch::error::SendError;
use tracing::{debug, trace};

pub fn default<M>(make: M, rng: SmallRng) -> (Service<M>, Update) {
    let routes = Routes::Forward(None);
    let (tx, rx) = watch::channel(routes.clone());
    let concrete = Service {
        make,
        routes: routes.clone(),
        updates: rx.clone(),
        rng,
    };
    let update = Update { tx, routes };
    (concrete, update)
}

#[derive(Clone, Debug)]
pub struct Service<M> {
    make: M,
    routes: Routes,
    updates: watch::Receiver<Routes>,
    rng: SmallRng,
}

#[derive(Debug)]
pub struct Update {
    routes: Routes,
    tx: watch::Sender<Routes>,
}

#[derive(Clone)]
enum Routes {
    Forward(Option<Addr>),
    Override {
        distribution: WeightedIndex<u32>,
        overrides: Vec<Addr>,
    },
}

impl std::fmt::Debug for Routes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Routes::Forward(None) => write!(f, "Routes::Forward"),
            Routes::Forward(Some(addr)) => write!(f, "Routes::Forward({})", addr),
            Routes::Override { overrides, .. } => {
                write!(f, "Routes::Override(")?;
                let mut addrs = overrides.iter();
                if let Some(a) = addrs.next() {
                    write!(f, "{}", a)?;
                }
                for a in addrs {
                    write!(f, ", {}", a)?;
                }
                write!(f, ")")
            }
        }
    }
}

impl<T, S> tower::Service<T> for Service<S>
where
    T: OverrideDestination,
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            match self.updates.poll_recv_ref(cx) {
                Poll::Pending | Poll::Ready(None) => break,
                Poll::Ready(Some(routes)) => {
                    debug!(?routes, "updated");
                    self.routes = (*routes).clone();
                }
            }
        }

        self.make.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut target: T) -> Self::Future {
        match self.routes {
            Routes::Forward(None) => {}
            Routes::Forward(Some(ref addr)) => {
                *target.dst_mut() = addr.clone().into();
            }
            Routes::Override {
                ref distribution,
                ref overrides,
            } => {
                debug_assert!(overrides.len() > 1);
                let idx = distribution.sample(&mut self.rng);
                debug_assert!(idx < overrides.len());
                *target.dst_mut() = overrides[idx].clone().into();
            }
        }

        self.make.call(target).map_err(Into::into)
    }
}

impl Update {
    pub fn set_forward(&mut self) -> Result<(), error::LostService> {
        if let Routes::Forward(None) = self.routes {
            trace!("default forward already set");
            return Ok(());
        };

        trace!("building default forward");
        self.routes = Routes::Forward(None);

        self.tx
            .broadcast(self.routes.clone())
            .map_err(|_| error::LostService(()))
    }

    pub fn set_split(&mut self, mut addrs: Vec<Target>) -> Result<(), error::LostService> {
        let routes = match self.routes {
            Routes::Forward(ref addr) => {
                if addrs.len() == 1 {
                    let new_addr = addrs.pop().unwrap().addr;
                    if addr.as_ref().map(|a| a == &new_addr).unwrap_or(false) {
                        trace!("forward already set to {}", new_addr);
                        return Ok(());
                    }
                    Routes::Forward(Some(new_addr))
                } else {
                    let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                        .expect("invalid weight distribution");
                    let overrides = addrs.into_iter().map(|w| w.addr).collect();
                    Routes::Override {
                        distribution,
                        overrides,
                    }
                }
            }
            Routes::Override { .. } => {
                if addrs.len() == 1 {
                    let new_addr = addrs.pop().unwrap().addr;
                    Routes::Forward(Some(new_addr))
                } else {
                    let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                        .expect("invalid weight distribution");
                    let overrides = addrs.into_iter().map(|w| w.addr).collect();
                    Routes::Override {
                        distribution,
                        overrides,
                    }
                }
            }
        };

        self.routes = routes.clone();
        self.tx
            .broadcast(routes)
            .map_err(|_| error::LostService(()))
    }
}

pub mod error {
    #[derive(Debug)]
    pub struct LostService(pub(super) ());

    impl std::fmt::Display for LostService {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "addrs lost")
        }
    }

    impl std::error::Error for LostService {}
}
