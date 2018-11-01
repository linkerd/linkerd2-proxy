//! Configures and runs the linkerd2 service sidecar proxy

use convert::TryFrom;
use logging;

mod classify;
pub mod config;
mod control;
mod destination;
mod inbound;
mod main;
mod metric_labels;
mod outbound;

use self::config::{Config, Env};
use self::destination::{Destination, NameOrAddr};
pub use self::main::Main;

pub fn init() -> Result<Config, config::Error> {
    logging::init();
    let config_strings = Env;
    Config::try_from(&config_strings)
}
