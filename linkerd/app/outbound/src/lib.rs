//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

pub mod http;
mod resolve;
pub mod server;
pub mod target;
pub mod tcp;
#[cfg(test)]
mod test_util;

use linkerd2_app_core::{config::ProxyConfig, metrics, IpMatch};
use std::{collections::HashMap, time::Duration};

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub allow_discovery: IpMatch,
}

fn stack_labels(name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::outbound(name)
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}
