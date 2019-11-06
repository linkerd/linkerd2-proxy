use indexmap::IndexSet;
use linkerd2_app_core::{
    dns::Suffix,
    dst::DstAddr,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::{api_resolve as api, resolve::recover},
    request_filter, Error, Recover,
};
use std::sync::Arc;
use tower_grpc::{generic::client::GrpcService, Body, BoxBody, Code, Status};

pub type Resolve<S> = request_filter::Service<
    PermitNamesInSuffixes,
    recover::Resolve<BackoffUnlessInvalidArgument, api::Resolve<S>>,
>;

pub fn new<S>(
    service: S,
    suffixes: Vec<Suffix>,
    token: &str,
    backoff: ExponentialBackoff,
) -> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    request_filter::Service::new::<DstAddr>(
        PermitNamesInSuffixes::new(suffixes),
        recover::Resolve::new::<DstAddr>(
            backoff.into(),
            api::Resolve::new::<DstAddr>(service).with_context_token(token),
        ),
    )
}

#[derive(Clone, Debug)]
pub struct PermitNamesInSuffixes {
    permitted: Arc<IndexSet<Suffix>>,
}

#[derive(Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

#[derive(Debug)]
pub struct Unresolvable(());

// === impl PermitNamesInSuffixes ===

impl PermitNamesInSuffixes {
    fn new(permitted: impl IntoIterator<Item = Suffix>) -> Self {
        Self {
            permitted: Arc::new(permitted.into_iter().collect()),
        }
    }
}

impl request_filter::RequestFilter<DstAddr> for PermitNamesInSuffixes {
    type Error = Unresolvable;

    fn filter(&self, dst: DstAddr) -> Result<DstAddr, Self::Error> {
        if let Some(name) = dst.dst_concrete().name_addr() {
            if self
                .permitted
                .iter()
                .any(|suffix| suffix.contains(name.name()))
            {
                tracing::debug!("suffix matches");
                return Ok(dst);
            }
        }

        Err(Unresolvable(()))
    }
}

// === impl Unresolvable ===

impl std::fmt::Display for Unresolvable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unresolvable")
    }
}

impl std::error::Error for Unresolvable {}

// === impl BackoffUnlessInvalidArgument ===

impl From<ExponentialBackoff> for BackoffUnlessInvalidArgument {
    fn from(eb: ExponentialBackoff) -> Self {
        BackoffUnlessInvalidArgument(eb)
    }
}

impl Recover<Error> for BackoffUnlessInvalidArgument {
    type Backoff = ExponentialBackoffStream;
    type Error = <ExponentialBackoffStream as futures::Stream>::Error;

    fn recover(&self, err: Error) -> Result<Self::Backoff, Error> {
        match err.downcast::<Status>() {
            Ok(ref status) if status.code() == Code::InvalidArgument => {
                tracing::debug!(message = "cannot recover", %status);
                return Err(Unresolvable(()).into());
            }
            Ok(status) => tracing::debug!(message = "recovering", %status),
            Err(error) => tracing::debug!(message = "recovering", %error),
        }

        Ok(self.0.stream())
    }
}
