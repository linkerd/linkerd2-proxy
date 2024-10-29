use super::*;
use linkerd_proxy_client_policy::{
    http, opaq, Backend, BackendDispatcher, ClientPolicy, EndpointDiscovery, Load, Meta, PeakEwma,
    Protocol, Queue, RouteBackend, RouteDistribution,
};
use std::time::Duration;

pub type ClientPolicies = Resolver<Addr, watch::Receiver<client_policy::ClientPolicy>>;

#[derive(Debug, Clone, Default)]
pub struct OutboundDiscover {
    pub profiles: Profiles,
    pub policies: ClientPolicies,
}

pub fn client_policy() -> ClientPolicies {
    ClientPolicies::default()
}

// === client policy resolver ===

impl ClientPolicies {
    pub fn policy_tx(&self, addr: impl Into<Addr>) -> watch::Sender<ClientPolicy> {
        // XXX(eliza): is this the best initial value?
        let init = ClientPolicy::invalid(Duration::from_secs(10));
        let (tx, rx) = watch::channel(init);
        self.state.endpoints.lock().insert(addr.into(), rx);
        tx
    }

    pub fn policy(self, addr: impl Into<Addr>, policy: client_policy::ClientPolicy) -> Self {
        let (tx, rx) = watch::channel(policy);
        self.state.unused_senders.lock().push(Box::new(tx));
        self.state.endpoints.lock().insert(addr.into(), rx);
        self
    }

    pub fn policy_default(self, addr: impl Into<Addr>) -> Self {
        let addr = addr.into();

        let parent = Meta::new_default(addr.to_string());

        let backend = {
            let dispatcher = match addr {
                Addr::Name(ref addr) => {
                    let load = Load::PeakEwma(PeakEwma {
                        default_rtt: Duration::from_millis(30),
                        decay: Duration::from_secs(10),
                    });
                    let disco = EndpointDiscovery::DestinationGet {
                        path: addr.to_string(),
                    };
                    BackendDispatcher::BalanceP2c(load, disco)
                }
                Addr::Socket(addr) => BackendDispatcher::Forward(addr, Default::default()),
            };
            let queue = Queue {
                capacity: 100,
                failfast_timeout: Duration::from_secs(10),
            };
            Backend {
                meta: Meta::new_default("test"),
                queue,
                dispatcher,
            }
        };

        let http_routes = Arc::new([http::Route {
            hosts: vec![],
            rules: vec![linkerd_http_route::Rule {
                matches: vec![],
                policy: http::Policy {
                    meta: Meta::new_default("default"),
                    filters: Arc::new([]),
                    params: Default::default(),
                    distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                        filters: Arc::new([]),
                        backend: backend.clone(),
                    }])),
                },
            }],
        }]);

        let protocol = Protocol::Detect {
            timeout: Duration::from_secs(10),
            http1: http::Http1 {
                routes: http_routes.clone(),
                failure_accrual: Default::default(),
            },
            http2: http::Http2 {
                routes: http_routes,
                failure_accrual: Default::default(),
            },
            opaque: opaq::Opaque {
                routes: Arc::new([opaq::Route {
                    policy: opaq::Policy {
                        meta: Meta::new_default("default"),
                        filters: Arc::new([]),
                        params: Default::default(),
                        distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                            filters: Arc::new([]),
                            backend: backend.clone(),
                        }])),
                    },
                }]),
            },
        };
        let policy = ClientPolicy {
            parent,
            protocol,
            backends: Arc::new([backend]),
        };
        self.policy(addr, policy)
    }

    fn resolve(&self, addr: Addr) -> Result<watch::Receiver<ClientPolicy>, Error> {
        let span = tracing::trace_span!("mock_policy", %addr);
        let _e = span.enter();

        self.state
            .endpoints
            .lock()
            .remove(&addr)
            .map(|x| {
                tracing::trace!("found policy for addr");
                x
            })
            .ok_or_else(|| {
                tracing::debug!(?addr, "no policy configured for");
                tonic::Status::not_found("no policy found for address").into()
            })
    }
}

impl<T: Param<Addr>> tower::Service<T> for ClientPolicies {
    type Response = watch::Receiver<ClientPolicy>;
    type Error = Error;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        future::ready(self.resolve(t.param()))
    }
}

// === impl OutboundDiscover ===

impl OutboundDiscover {
    pub fn with_default(self, addr: impl Into<Addr>) -> Self {
        let addr = addr.into();
        let policies = self.policies.policy_default(addr.clone());
        let profiles = self.profiles.profile(addr, Default::default());
        Self { profiles, policies }
    }
}

impl<T> tower::Service<T> for OutboundDiscover
where
    T: Into<Addr>,
{
    type Response = (Option<profiles::Receiver>, watch::Receiver<ClientPolicy>);
    type Error = Error;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let addr = t.into();
        let policy = match self.policies.resolve(addr.clone()) {
            Ok(policy) => policy,
            Err(error) => return future::err(error),
        };
        let profile = self.profiles.resolve(addr);
        future::ok((profile, policy))
    }
}
