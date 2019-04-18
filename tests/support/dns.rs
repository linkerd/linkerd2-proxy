pub use support::linkerd2_proxy::dns::*;
use support::tokio::prelude::future;

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Instant, Duration},iter,
};

pub fn new() -> MockDns {
    MockDns {
        inner: Arc::new(Mutex::new(Inner {
            resolutions: HashMap::new(),
            refines: HashMap::new(),
        }))
    }
}

#[derive(Clone)]
pub struct MockDns {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
pub struct MockIpList {
    ip: IpAddr,
    valid_until: Instant,
}

struct Inner {
    resolutions: HashMap<String, IpAddr>,
    refines: HashMap<String, Name>,
}

impl MockDns {
    pub fn refine(self, name: &str, refined: &str) -> Self {
        let name = name.to_string();
        let fqdn = linkerd2_proxy::convert::TryFrom::try_from(refined.as_bytes()).expect("bad dns name");
        self.inner.lock().unwrap().refines.insert(name, fqdn);
        self
    }

    pub fn resolve(self, name: &str, addr: impl Into<IpAddr>) -> Self {
        let name = name.to_string();
        self.inner.lock().unwrap().resolutions.insert(name, addr.into());
        self
    }
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
        println!("refine: {}", name);
        let lock = self.inner.lock().unwrap();
        if let Some(name) = lock.refines.get(name.without_trailing_dot()).cloned() {
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
    type List = MockIpList;
    type ListFuture = future::FutureResult<Response<Self::List>, ResolveError>;

    fn resolve_one_ip(&self, name: &Name) -> Self::Future {
        let lock = self.inner.lock().unwrap();
        if let Some(addr) = lock.resolutions.get(name.without_trailing_dot()).cloned() {
            return future::ok(addr);
        } else {
            future::err(Error::NoAddressesFound)
        }
    }

    fn resolve_all_ips(&self, dead: Instant, name: &Name) -> Self::ListFuture {
        println!("resolve_all_ips: {}", name);
        let lock = self.inner.lock().unwrap();
        if let Some(ip) = lock.resolutions.get(name.without_trailing_dot()).cloned() {
            future::ok(Response::Exists(MockIpList {
            ip,
            // TODO: customize?
            valid_until: (dead + Duration::from_secs(666))
        }))
        } else {
            future::ok(Response::DoesNotExist { retry_after: Some(Instant::now() + Duration::from_secs(6000)) })
        }
    }
}

impl<'a> IpList<'a> for MockIpList {
    type Iter = Box<Iterator<Item=IpAddr>>;

    fn iter(&'a self) -> Self::Iter {
        Box::new(iter::once(self.ip.clone()))
    }

    fn valid_until(&self) -> Instant {
        Instant::now() + Duration::from_secs(666)
    }
}