use super::endpoint;
use crate::api_resolve::Metadata;
use crate::app::dst::DstAddr;
use crate::dns::Suffix;
use futures::{try_ready, Future, Poll, Stream};
use linkerd2_proxy_core::{Error, Recover, Resolve};
use linkerd2_proxy_resolve::{map_endpoint, recover, reject_targets};
use std::time::{Duration, Instant};
use tokio::timer::{self, Delay};
use tower_grpc as grpc;

pub fn resolve<R>(
    suffixes: Vec<Suffix>,
    backoff: ExponentialBackoff,
    resolve: R,
) -> recover::Resolve<
    BackoffUnlessUnresolvable,
    map_endpoint::Resolve<
        endpoint::FromMetadata,
        reject_targets::Resolve<PermitNamesInSuffixes, R>,
    >,
>
where
    R: Resolve<DstAddr, Endpoint = Metadata> + Clone,
{
    recover::Resolve::new(
        backoff.into(),
        map_endpoint::Resolve::new(
            endpoint::FromMetadata,
            reject_targets::Resolve::new(suffixes.into(), resolve),
        ),
    )
}

#[derive(Clone, Debug)]
pub struct PermitNamesInSuffixes {
    permitted: Vec<Suffix>,
}

#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
    min: Duration,
    max: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct BackoffUnlessUnresolvable(ExponentialBackoff);

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
        Self { permitted }
    }
}

impl reject_targets::CheckTarget<DstAddr> for PermitNamesInSuffixes {
    type Error = Unresolvable;

    fn check_target(&self, dst: &DstAddr) -> Result<(), Self::Error> {
        if let Some(name) = dst.dst_concrete().name_addr() {
            if self
                .permitted
                .iter()
                .any(|suffix| suffix.contains(name.name()))
            {
                return Ok(());
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

// === impl BackoffUnlessUnresolvable ===

impl From<ExponentialBackoff> for BackoffUnlessUnresolvable {
    fn from(eb: ExponentialBackoff) -> Self {
        BackoffUnlessUnresolvable(eb)
    }
}

impl Recover<Error> for BackoffUnlessUnresolvable {
    type Backoff = BackoffStream;
    type Error = timer::Error;

    fn recover(&self, err: Error) -> Result<Self::Backoff, Error> {
        if err.downcast_ref::<Unresolvable>().is_some() {
            return Err(err);
        }

        if let Some(status) = err.downcast::<grpc::Status>().ok() {
            if status.code() == grpc::Code::InvalidArgument {
                return Err(Unresolvable(()).into());
            }
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
