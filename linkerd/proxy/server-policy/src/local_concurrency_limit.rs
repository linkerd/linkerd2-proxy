use crate::Meta;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// A concurrency limiter that can be shared across connections within a proxy pod.
/// 
/// This is similar to [`LocalRateLimit`](crate::LocalRateLimit) but limits the number
/// of concurrent in-flight requests rather than the rate of requests.
#[derive(Debug)]
pub struct LocalConcurrencyLimit {
    meta: Option<Arc<Meta>>,
    /// Maximum number of concurrent requests
    max: usize,
    /// Semaphore to track in-flight requests
    semaphore: Arc<Semaphore>,
}

/// Error returned when the concurrency limit is exceeded.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("concurrency limit exceeded: max {0} in-flight requests")]
pub struct ConcurrencyLimitError(pub usize);

// === impl LocalConcurrencyLimit ===

impl LocalConcurrencyLimit {
    /// Creates a new concurrency limit with the given maximum.
    pub fn new(max: usize, meta: Option<Arc<Meta>>) -> Self {
        Self {
            meta,
            max,
            semaphore: Arc::new(Semaphore::new(max)),
        }
    }

    /// Try to acquire a concurrency permit (non-blocking).
    /// 
    /// The returned permit MUST be held until the response (including body)
    /// is fully consumed. Dropping the permit releases the slot.
    /// 
    /// Returns `Err` if the concurrency limit is reached.
    pub fn try_acquire(&self) -> Result<OwnedSemaphorePermit, ConcurrencyLimitError> {
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|e| match e {
                TryAcquireError::NoPermits => ConcurrencyLimitError(self.max),
                TryAcquireError::Closed => ConcurrencyLimitError(self.max),
            })
    }

    /// Returns the metadata associated with this concurrency limit.
    pub fn meta(&self) -> Option<Arc<Meta>> {
        self.meta.clone()
    }

    /// Returns the maximum number of concurrent requests allowed.
    pub fn max(&self) -> usize {
        self.max
    }

    /// Returns the number of available permits.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

impl Default for LocalConcurrencyLimit {
    fn default() -> Self {
        // Default to no limit (max permits)
        Self {
            meta: None,
            max: Semaphore::MAX_PERMITS,
            semaphore: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        }
    }
}

#[cfg(feature = "test-util")]
impl LocalConcurrencyLimit {
    /// Creates a new concurrency limit for testing.
    pub fn new_for_test(max: usize) -> Self {
        Self::new(max, None)
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::meta::proto::InvalidMeta;
    use linkerd2_proxy_api::inbound as api;

    impl TryFrom<api::HttpLocalConcurrencyLimit> for LocalConcurrencyLimit {
        type Error = InvalidMeta;

        fn try_from(proto: api::HttpLocalConcurrencyLimit) -> Result<Self, Self::Error> {
            let meta = proto
                .metadata
                .map(Meta::try_from)
                .transpose()?
                .map(Arc::new);
            let max = proto.max_in_flight_requests as usize;

            // If max is 0, treat as unlimited
            if max == 0 {
                return Ok(Self::default());
            }

            Ok(Self::new(max, meta))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_concurrency_limit_basic() {
        let limit = LocalConcurrencyLimit::new(2, None);

        // Acquire first permit
        let permit1 = limit.try_acquire().expect("should acquire first permit");
        assert_eq!(limit.available_permits(), 1);

        // Acquire second permit
        let permit2 = limit.try_acquire().expect("should acquire second permit");
        assert_eq!(limit.available_permits(), 0);

        // Third should fail
        let result = limit.try_acquire();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ConcurrencyLimitError(2));

        // Drop first permit, should be able to acquire again
        drop(permit1);
        assert_eq!(limit.available_permits(), 1);

        let _permit3 = limit.try_acquire().expect("should acquire after release");
        assert_eq!(limit.available_permits(), 0);

        drop(permit2);
        assert_eq!(limit.available_permits(), 1);
    }

    #[test]
    fn test_default_is_unlimited() {
        let limit = LocalConcurrencyLimit::default();
        assert_eq!(limit.max(), Semaphore::MAX_PERMITS);

        // Should be able to acquire many permits
        let permits: Vec<_> = (0..100)
            .map(|_| limit.try_acquire().expect("should acquire"))
            .collect();

        assert_eq!(permits.len(), 100);
    }

    #[test]
    fn test_concurrency_limit_single() {
        let limit = LocalConcurrencyLimit::new(1, None);
        assert_eq!(limit.max(), 1);
        assert_eq!(limit.available_permits(), 1);

        let permit = limit.try_acquire().expect("should acquire");
        assert_eq!(limit.available_permits(), 0);

        // Should fail with correct error
        let err = limit.try_acquire().unwrap_err();
        assert_eq!(err, ConcurrencyLimitError(1));

        drop(permit);
        assert_eq!(limit.available_permits(), 1);
    }

    #[test]
    fn test_concurrency_limit_high_load() {
        let limit = LocalConcurrencyLimit::new(100, None);

        // Acquire all permits
        let permits: Vec<_> = (0..100)
            .map(|_| limit.try_acquire().expect("should acquire"))
            .collect();

        assert_eq!(limit.available_permits(), 0);

        // 101st should fail
        let err = limit.try_acquire().unwrap_err();
        assert_eq!(err, ConcurrencyLimitError(100));

        // Release all permits
        drop(permits);
        assert_eq!(limit.available_permits(), 100);
    }

    #[test]
    fn test_meta() {
        let meta = Arc::new(Meta::Default { name: "test-limit".into() });
        let limit = LocalConcurrencyLimit::new(10, Some(meta.clone()));

        assert!(limit.meta().is_some());
        assert!(matches!(limit.meta().unwrap().as_ref(), Meta::Default { name } if name == "test-limit"));
    }

    #[cfg(feature = "proto")]
    mod proto_tests {
        use super::*;
        use linkerd2_proxy_api::{inbound, meta};

        #[test]
        fn from_proto_basic() {
            let proto = inbound::HttpLocalConcurrencyLimit {
                metadata: Some(meta::Metadata {
                    kind: Some(meta::metadata::Kind::Default("concurrency-limit-1".into())),
                }),
                max_in_flight_requests: 50,
            };

            let cl: LocalConcurrencyLimit = proto.try_into().unwrap();
            assert_eq!(cl.max(), 50);
            assert!(cl.meta().is_some());
        }

        #[test]
        fn from_proto_zero_is_unlimited() {
            let proto = inbound::HttpLocalConcurrencyLimit {
                metadata: None,
                max_in_flight_requests: 0,
            };

            let cl: LocalConcurrencyLimit = proto.try_into().unwrap();
            assert_eq!(cl.max(), Semaphore::MAX_PERMITS);
            assert!(cl.meta().is_none());
        }

        #[test]
        fn from_proto_no_metadata() {
            let proto = inbound::HttpLocalConcurrencyLimit {
                metadata: None,
                max_in_flight_requests: 100,
            };

            let cl: LocalConcurrencyLimit = proto.try_into().unwrap();
            assert_eq!(cl.max(), 100);
            assert!(cl.meta().is_none());
        }

        #[test]
        fn from_proto_with_resource_metadata() {
            let proto = inbound::HttpLocalConcurrencyLimit {
                metadata: Some(meta::Metadata {
                    kind: Some(meta::metadata::Kind::Resource(meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "HTTPLocalConcurrencyLimitPolicy".into(),
                        name: "my-concurrency-limit".into(),
                        namespace: "default".into(),
                        section: "".into(),
                        port: 0,
                    })),
                }),
                max_in_flight_requests: 25,
            };

            let cl: LocalConcurrencyLimit = proto.try_into().unwrap();
            assert_eq!(cl.max(), 25);
            assert!(cl.meta().is_some());
        }
    }
}
