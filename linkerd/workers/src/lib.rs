//! Core allocation logic for Linkerd's worker threads.

use std::num::NonZeroUsize;

/// Determines the number of worker threads to use in a runtime.
#[derive(Copy, Clone, Debug)]
pub struct Workers {
    pub available: NonZeroUsize,
    pub max_ratio: Option<f64>,
    pub max_cores: Option<NonZeroUsize>,
    pub min_cores: NonZeroUsize,
}

impl Workers {
    /// Calculate the number of cores to use based on the constraints.
    ///
    /// The algorithm uses the following precedence:
    /// 1. The explicitly configured maximum cores, if present
    /// 2. The ratio-based calculation, if present
    /// 3. Default to 1 core
    ///
    /// The result is constrained by both the minimum cores and the available cores.
    pub fn cores(&self) -> NonZeroUsize {
        let Self {
            available,
            max_ratio,
            max_cores,
            min_cores,
        } = *self;

        max_cores
            .or_else(|| {
                max_ratio.and_then(|ratio| {
                    let max = (available.get() as f64 * ratio).round() as usize;
                    max.try_into().ok()
                })
            })
            .unwrap_or_else(|| 1.try_into().unwrap())
            .max(min_cores)
            .min(available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_cores_exceeds_max_cores() {
        let workers = Workers {
            available: NonZeroUsize::new(8).unwrap(),
            max_cores: NonZeroUsize::new(2),
            min_cores: NonZeroUsize::new(4).unwrap(),
            max_ratio: None,
        };
        assert_eq!(workers.cores().get(), 4);
    }

    #[test]
    fn available_limits_max_cores() {
        let workers = Workers {
            available: NonZeroUsize::new(2).unwrap(),
            max_cores: NonZeroUsize::new(4),
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: None,
        };
        assert_eq!(workers.cores().get(), 2);
    }

    #[test]
    fn max_ratio_calculates_cores() {
        let workers = Workers {
            available: NonZeroUsize::new(10).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: Some(0.5),
        };
        assert_eq!(workers.cores().get(), 5); // 10 * 0.5 = 5
    }

    #[test]
    fn max_cores_overrides_ratio() {
        let workers = Workers {
            available: NonZeroUsize::new(10).unwrap(),
            max_cores: NonZeroUsize::new(3),
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: Some(0.5),
        };
        assert_eq!(workers.cores().get(), 3);
    }

    #[test]
    fn min_cores_exceeds_ratio_calculation() {
        let workers = Workers {
            available: NonZeroUsize::new(10).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(6).unwrap(),
            max_ratio: Some(0.5),
        };
        assert_eq!(workers.cores().get(), 6); // min_cores > max_cores from ratio (5)
    }

    #[test]
    fn fallback_to_min_cores_when_no_max() {
        let workers = Workers {
            available: NonZeroUsize::new(8).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(2).unwrap(),
            max_ratio: None,
        };
        assert_eq!(workers.cores().get(), 2);
    }

    #[test]
    fn single_cpu_environment() {
        let workers = Workers {
            available: NonZeroUsize::new(1).unwrap(),
            max_cores: NonZeroUsize::new(4),
            min_cores: NonZeroUsize::new(2).unwrap(),
            max_ratio: None,
        };
        assert_eq!(workers.cores().get(), 1);
    }

    #[test]
    fn ratio() {
        // For 10 CPUs with 0.31 ratio, we get 3.1 cores, which rounds to 3
        let workers = Workers {
            available: NonZeroUsize::new(10).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: Some(0.31),
        };
        assert_eq!(workers.cores().get(), 3);

        // For 10 CPUs with 0.35 ratio, we get 3.5 cores, which rounds to 4
        let workers = Workers {
            available: NonZeroUsize::new(10).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: Some(0.35),
        };
        assert_eq!(workers.cores().get(), 4);

        // For 8 CPUs with 0.25 ratio, we get exactly 2 cores
        let workers = Workers {
            available: NonZeroUsize::new(8).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: Some(0.25),
        };
        assert_eq!(workers.cores().get(), 2);

        // For 96 CPUs with 1.0 ratio, we get all 96 cores
        let workers = Workers {
            available: NonZeroUsize::new(96).unwrap(),
            max_cores: None,
            min_cores: NonZeroUsize::new(1).unwrap(),
            max_ratio: Some(1.0),
        };
        assert_eq!(workers.cores().get(), 96);
    }
}
