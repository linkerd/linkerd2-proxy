use super::endpoint;
use crate::api_resolve::Metadata;
use crate::app::dst::DstAddr;
use crate::dns::Suffix;
use futures::{try_ready, Future, Poll, Stream};
use linkerd2_proxy_core::{resolve, Error, Recover, Resolve};
use linkerd2_proxy_resolve::{map_endpoint, recover};
use linkerd2_request_filter as request_filter;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::timer::{self, Delay};
use tower_grpc as grpc;

pub fn resolve<R>(
    suffixes: Vec<Suffix>,
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
    permitted: Arc<Vec<Suffix>>,
}

#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
    min: Duration,
    max: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

#[derive(Debug)]
pub struct BackoffStream {
    backoff: ExponentialBackoff,
    iterations: u32,
    delay: Option<Delay>,
}

#[derive(Debug)]
pub struct Unresolvable(());

// === impl PermitNamesInSuffixes ===

impl From<Vec<Suffix>> for PermitNamesInSuffixes {
    fn from(permitted: Vec<Suffix>) -> Self {
        Self {
            permitted: Arc::new(permitted),
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

// === impl ExponentialBackoff ===

impl ExponentialBackoff {
    const DEFAULT_MIN: Duration = Duration::from_millis(100);
    const DEFAULT_MAX: Duration = Duration::from_secs(10);

    pub fn new(min: Duration, max: Duration) -> Self {
        Self { min, max }
    }
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MIN, Self::DEFAULT_MAX)
    }
}

// === impl BackoffUnlessInvalidArgument ===

impl From<ExponentialBackoff> for BackoffUnlessInvalidArgument {
    fn from(eb: ExponentialBackoff) -> Self {
        BackoffUnlessInvalidArgument(eb)
    }
}

impl Recover<Error> for BackoffUnlessInvalidArgument {
    type Backoff = BackoffStream;
    type Error = timer::Error;

    fn recover(&self, err: Error) -> Result<Self::Backoff, Error> {
        match err.downcast::<grpc::Status>() {
            Ok(ref status) if status.code() == grpc::Code::InvalidArgument => {
                tracing::debug!(message = "cannot recover", %status);
                return Err(Unresolvable(()).into());
            }
            Ok(status) => tracing::debug!(message = "recovering", %status),
            Err(error) => tracing::debug!(message = "recovering", %error),
        }

        Ok(Self::Backoff {
            backoff: self.0.clone(),
            iterations: 0,
            delay: None,
        })
    }
}

// === impl BackoffStream ===

impl Stream for BackoffStream {
    type Item = ();
    type Error = timer::Error;

    fn poll(&mut self) -> Poll<Option<()>, Self::Error> {
        use std::ops::Mul;

        loop {
            if let Some(delay) = self.delay.as_mut() {
                try_ready!(delay.poll());

                self.delay = None;
                self.iterations += 1;
                return Ok(Some(()).into());
            }

            let factor = 2u32.saturating_pow(self.iterations + 1);
            let backoff = self
                .backoff
                .min
                .clone()
                .mul(factor)
                .min(self.backoff.max.clone());
            self.delay = Some(Delay::new(Instant::now() + backoff));
        }
    }
}
