use super::endpoint;
use indexmap::IndexSet;
use linkerd2_app_core::{
    dns::Suffix,
    dst::DstAddr,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::{
        api_resolve::Metadata,
        core::{resolve, Resolve},
        resolve::{map_endpoint, recover},
    },
    request_filter, Error, Recover,
};
use std::sync::Arc;
use tower_grpc as grpc;

pub fn resolve<R, S>(
    suffixes: S,
    backoff: ExponentialBackoff,
    resolve: R,
) -> map_endpoint::Resolve<
    endpoint::FromMetadata,
    request_filter::Service<
        PermitNamesInSuffixes,
        resolve::Service<recover::Resolve<BackoffUnlessInvalidArgument, R>>,
    >,
>
where
    R: Resolve<DstAddr, Endpoint = Metadata> + Clone,
    S: IntoIterator<Item = Suffix>,
{
    map_endpoint::Resolve::new(
        endpoint::FromMetadata,
        request_filter::Service::new(
            suffixes.into(),
            recover::Resolve::new(backoff.into(), resolve).into_service(),
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

impl<I: IntoIterator<Item = Suffix>> From<I> for PermitNamesInSuffixes {
    fn from(permitted: I) -> Self {
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
        match err.downcast::<grpc::Status>() {
            Ok(ref status) if status.code() == grpc::Code::InvalidArgument => {
                tracing::debug!(message = "cannot recover", %status);
                return Err(Unresolvable(()).into());
            }
            Ok(status) => tracing::debug!(message = "recovering", %status),
            Err(error) => tracing::debug!(message = "recovering", %error),
        }

        Ok(self.0.stream())
    }
}
