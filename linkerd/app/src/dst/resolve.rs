use http_body::Body as HttpBody;
use ipnet::{Contains, IpNet};
use linkerd2_app_core::{
    dns::Suffix,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::{
        api_resolve as api,
        resolve::{self, recover},
    },
    request_filter, Addr, DiscoveryRejected, Error, Recover,
};
use linkerd2_app_outbound::Target;
use linkerd2_error::Never;
use std::net::IpAddr;
use std::sync::Arc;
use tonic::{
    body::{Body, BoxBody},
    client::GrpcService,
    Code, Status,
};

pub type Resolve<S> = request_filter::Service<
    PermitConfiguredDsts,
    recover::Resolve<BackoffUnlessInvalidArgument, resolve::make_unpin::Resolve<api::Resolve<S>>>,
>;

pub fn new<S>(
    service: S,
    suffixes: impl IntoIterator<Item = Suffix>,
    nets: impl IntoIterator<Item = IpNet>,
    token: &str,
    backoff: ExponentialBackoff,
) -> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    S::Future: Send,
{
    request_filter::Service::new(
        PermitConfiguredDsts::new(suffixes, nets),
        recover::Resolve::new(
            backoff.into(),
            resolve::make_unpin(api::Resolve::new(service).with_context_token(token)),
        ),
    )
}

#[derive(Clone, Debug)]
pub struct PermitConfiguredDsts {
    name_suffixes: Arc<Vec<Suffix>>,
    networks: Arc<Vec<IpNet>>,
}

#[derive(Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

// === impl PermitConfiguredDsts ===

impl PermitConfiguredDsts {
    fn new(
        name_suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self {
            name_suffixes: Arc::new(name_suffixes.into_iter().collect()),
            networks: Arc::new(nets.into_iter().collect()),
        }
    }
}

impl<T> request_filter::RequestFilter<Target<T>> for PermitConfiguredDsts {
    type Error = DiscoveryRejected;

    fn filter(&self, t: Target<T>) -> Result<Target<T>, Self::Error> {
        let permitted = match t.addr {
            Addr::Name(ref name) => self
                .name_suffixes
                .iter()
                .any(|suffix| suffix.contains(name.name())),
            Addr::Socket(sa) => self.networks.iter().any(|net| match (net, sa.ip()) {
                (IpNet::V4(net), IpAddr::V4(addr)) => net.contains(&addr),
                (IpNet::V6(net), IpAddr::V6(addr)) => net.contains(&addr),
                _ => false,
            }),
        };

        if permitted {
            Ok(t)
        } else {
            Err(DiscoveryRejected::new())
        }
    }
}

// === impl BackoffUnlessInvalidArgument ===

impl From<ExponentialBackoff> for BackoffUnlessInvalidArgument {
    fn from(eb: ExponentialBackoff) -> Self {
        BackoffUnlessInvalidArgument(eb)
    }
}

impl Recover<Error> for BackoffUnlessInvalidArgument {
    type Backoff = ExponentialBackoffStream;
    type Error = Never;

    fn recover(&self, err: Error) -> Result<Self::Backoff, Error> {
        match err.downcast::<Status>() {
            Ok(ref status) if status.code() == Code::InvalidArgument => {
                tracing::debug!(message = "cannot recover", %status);
                return Err(DiscoveryRejected::new().into());
            }
            Ok(status) => tracing::trace!(message = "recovering", %status),
            Err(error) => tracing::trace!(message = "recovering", %error),
        }

        Ok(self.0.stream())
    }
}
