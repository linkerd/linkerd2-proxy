pub use support::linkerd2_proxy::dns::*;
use support::tokio::prelude::future;

use std::{
    collections::{VecDeque, HashMap},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Instant, Duration}
};

#[derive(Clone)]
pub struct MockDns {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    resolvutions: HashMap<String, VecDeque<IpAddr>>,
    refines: HashMap<String, VecDeque<Name>>,
}

impl NewResolver for MockDns {
    type Resolver = Self;
    type Background = future::FutureResult<(), ()>;

    /// NOTE: It would be nice to be able to return a named type rather than
    ///       `impl Future` for the background future; it would be called
    ///       `Background` or `ResolverBackground` if that were possible.
    fn new_resolver(
        &self,
        _: ResolverConfig,
        _: ResolverOpts,
    ) -> (Self, Self::Background) {
        (self.clone(), future::ok(()))
    }
}


impl Refine for MockDns {
    type Future = future::FutureResult<RefinedName, ResolveError>;

    fn refine(&self, name: &Name) -> Self::Future {
        let mut lock = self.inner.lock().unwrap();
        if let Some(name) = lock.refines.get_mut(name.without_trailing_dot()).and_then(|names| names.pop_front()) {
            return future::ok(RefinedName {
                name,
                // TODO: customize?
                valid_until: Instant::now() + Duration::from_secs(6000)
            });
        } else {
            // FIXME(eliza): make good
            future::err(ResolveError::from("fake nxdomain".to_string()))
        }
    }
}

impl Resolve for MockDns {
    type Future = future::FutureResult<IpAddr, Error>;
    type ListFuture = future::FutureResult<Response, ResolveError>;
    fn resolve_one_ip(&self, _: &Name) -> Self::Future {
        unimplemented!()
    }
    fn resolve_all_ips(&self, _: Instant, _: &Name) -> Self::ListFuture {
        unimplemented!()
    }
}