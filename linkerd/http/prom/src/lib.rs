#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod count_reqs;
// mod count_rsps;
pub mod request_duration;

pub use self::{
    count_reqs::{CountRequests, NewCountRequests, RequestCount, RequestCountFamilies},
    // count_rsps::{
    //     NewResponseMetrics, ResponseMetrics, ResponseMetricsFamilies, ResponseMetricsService,
    // },
    // request_duration::RequestDurationHistogram,
};
