use std::hash::Hash;

/// A filter that responds with an error at a predictable rate.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct InjectFailure<T = FailureResponse> {
    pub response: T,
    pub distribution: Distribution,
}

/// An HTTP error response.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FailureResponse {
    pub status: http::StatusCode,
    pub message: std::sync::Arc<str>,
}

/// A Bernoulli distribution that implements `Hash`, `PartialEq`, and `Eq`.
#[derive(Clone, Debug)]
pub struct Distribution {
    numerator: u32,
    denominator: u32,
    inner: rand::distributions::Bernoulli,
}

// === impl InjectFailure ===

impl<T: Clone> InjectFailure<T> {
    pub fn apply(&self) -> Option<T> {
        use rand::distributions::Distribution;

        if self.distribution.sample(&mut rand::thread_rng()) {
            return Some(self.response.clone());
        }

        None
    }
}

// === impl InjectFailure ===

impl Distribution {
    #[inline]
    pub fn from_ratio(
        numerator: u32,
        denominator: u32,
    ) -> Result<Self, rand::distributions::BernoulliError> {
        let inner = rand::distributions::Bernoulli::from_ratio(numerator, denominator)?;
        Ok(Self {
            numerator,
            denominator,
            inner,
        })
    }
}

impl Default for Distribution {
    fn default() -> Self {
        Self::from_ratio(1, 1).expect("default distribution must be valid")
    }
}

impl rand::distributions::Distribution<bool> for Distribution {
    #[inline]
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> bool {
        self.inner.sample(rng)
    }
}

impl PartialEq for Distribution {
    fn eq(&self, other: &Self) -> bool {
        self.numerator == other.numerator && self.denominator == other.denominator
    }
}

impl Eq for Distribution {}

impl Hash for Distribution {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.numerator.hash(state);
        self.denominator.hash(state);
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::http_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFailureResponse {
        #[error("invalid HTTP status code: {0}")]
        Status(#[from] http::status::InvalidStatusCode),

        #[error("HTTP status is not a u16")]
        StatusNonU16(#[from] std::num::TryFromIntError),

        #[error("{0}")]
        Distribution(#[from] InvalidDistribution),
    }

    #[derive(Debug, thiserror::Error)]
    #[error("invalid request distribution: {0}")]
    pub struct InvalidDistribution(#[from] rand::distributions::BernoulliError);

    // === impl InjectFailure ===

    impl TryFrom<api::HttpFailureInjector> for InjectFailure {
        type Error = InvalidFailureResponse;

        fn try_from(proto: api::HttpFailureInjector) -> Result<Self, Self::Error> {
            let response = FailureResponse {
                status: u16::try_from(proto.status)?.try_into()?,
                message: proto.message.into(),
            };

            Ok(InjectFailure {
                response,
                distribution: proto.ratio.try_into()?,
            })
        }
    }

    impl TryFrom<Option<api::Ratio>> for Distribution {
        type Error = InvalidDistribution;

        #[inline]
        fn try_from(p: Option<api::Ratio>) -> Result<Self, Self::Error> {
            Ok(p.map(TryInto::try_into).transpose()?.unwrap_or_default())
        }
    }

    impl TryFrom<api::Ratio> for Distribution {
        type Error = InvalidDistribution;

        #[inline]
        fn try_from(proto: api::Ratio) -> Result<Self, Self::Error> {
            Self::from_ratio(proto.numerator, proto.denominator).map_err(Into::into)
        }
    }
}
