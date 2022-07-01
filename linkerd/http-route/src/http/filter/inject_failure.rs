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
