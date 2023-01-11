use crate::Meta;
use std::{sync::Arc, time::Duration};

pub type Protocol = crate::Protocol<Distribution>;
pub type Policy = crate::Policy<Distribution>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Distribution {
    Forward(Backend),
    FirstAvailable(Arc<[Backend]>),
    RandomAvailable(Arc<[(Backend, u32)]>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backend {
    pub meta: Arc<Meta>,
    pub queue_capacity: usize,
    pub failfast_timeout: Duration,
    pub destination_path: String,
}

pub struct PeakEwmaLoadBalancer {
    pub default_rtt: Duration,
    pub decay: Duration,
}
